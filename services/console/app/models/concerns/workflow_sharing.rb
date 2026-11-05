module WorkflowSharing
  extend ActiveSupport::Concern

  included do
    has_many :workflow_grants, as: :resource, dependent: :destroy
    enum :visibility, { private: "private", shared: "shared" }, default: :private, validate: true, prefix: true
  end

  class_methods do
    def readable_by(user)
      return none unless user&.active?

      where(visibility: "shared").or(owned_by(user)).or(where(id: WorkflowGrant.where(resource_type: name, user: user).select(:resource_id)))
    end

    def editable_by(user)
      return none unless user&.active?

      owned_by(user).or(where(id: WorkflowGrant.where(resource_type: name, user: user, access: "editor").select(:resource_id)))
    end
  end

  def readable_by?(user)
    user&.active? && (visibility_shared? || editable_by?(user) || workflow_grants.exists?(user: user))
  end

  def editable_by?(user)
    user&.active? && (sharing_owner == user || workflow_grants.exists?(user: user, access: "editor"))
  end

  # Owners explicitly delegate editing and execution; ownership and runtime identity never change.
  def replace_access!(attributes)
    attributes = attributes.to_h.with_indifferent_access
    transaction do
      save! if new_record?
      lock!
      self.visibility = attributes[:visibility] if attributes.key?(:visibility)
      unless self.class.visibilities.key?(visibility)
        errors.add(:visibility, "is invalid")
        raise ActiveRecord::RecordInvalid, self
      end
      %w[reader editor].each do |access|
        key = "#{access}_oids"
        next unless attributes.key?(key)

        oids = Array(attributes[key]).compact_blank.uniq
        users = User.active.where(id: oids.filter_map { |oid| User.decode_oid(oid) }).to_a
        unless users.size == oids.size && users.none? { |user| user == sharing_owner }
          errors.add(:base, "Access includes an unavailable user or the owner")
          raise ActiveRecord::RecordInvalid, self
        end
        workflow_grants.where(access: access).destroy_all
        users.each { |user| workflow_grants.create!(user: user, access: access) }
      end
      # Revocation must remain possible even if a task's delivery destination is no longer valid.
      update_columns(visibility: visibility, updated_at: Time.current)
    end
  end

  def access_payload
    {
      visibility: visibility,
      owner: sharing_owner.oid,
      readers: workflow_grants.where(access: "reader").includes(:user).map { |grant| grant.user.oid },
      editors: workflow_grants.where(access: "editor").includes(:user).map { |grant| grant.user.oid }
    }
  end
end
