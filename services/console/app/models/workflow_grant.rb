class WorkflowGrant < ApplicationRecord
  belongs_to :resource, polymorphic: true
  belongs_to :user

  validates :resource_type, inclusion: { in: %w[ScheduledTask WorkflowAccess] }
  validates :access, inclusion: { in: %w[reader editor] }
  validates :user_id, uniqueness: { scope: %i[resource_type resource_id access] }
  validate :eligible_user

  private

  def eligible_user
    errors.add(:user, "must be active and cannot be the owner") unless user&.active? && user != resource&.sharing_owner
  end
end
