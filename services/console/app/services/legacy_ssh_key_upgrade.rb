# One-time bridge from the retired fork's encrypted SSH records. No legacy row
# is changed, and dry runs never create keys or grants.
class LegacySshKeyUpgrade
  class UnsafeState < StandardError; end

  class LegacyConnection < ApplicationRecord
    self.table_name = "ssh_connections"
    encrypts :private_key
  end

  def initialize(expected_active_keys: nil, deployment_policy_reviewed: false)
    @expected_active_keys = expected_active_keys
    @deployment_policy_reviewed = deployment_policy_reviewed
  end

  def call(apply: false)
    report = { mode: apply ? "apply" : "dry_run", legacy_rows: 0, eligible: 0,
               skipped_revoked: 0, skipped_expired: 0, skipped_inactive: 0,
               create: 0, already_upgraded: 0, expected_grants: 0 }
    return report.merge(status: "absent") unless LegacyConnection.table_exists?

    ApplicationRecord.transaction(requires_new: true) do
      ApplicationRecord.connection.execute("SET LOCAL lock_timeout = '5s'") if apply
      rows = LegacyConnection.order(:id)
      rows = rows.lock if apply
      plans = rows.map do |row|
        report[:legacy_rows] += 1
        plan_for(row, report, lock: apply)
      end.compact
      unless plans.map { |plan| plan[:user].id }.uniq.length == plans.length
        raise UnsafeState, "more than one eligible legacy key belongs to an owner"
      end
      if @expected_active_keys && report[:eligible] != Integer(@expected_active_keys)
        raise UnsafeState, "eligible key count differs from the reviewed expected count"
      end
      if apply
        raise UnsafeState, "apply requires an explicit expected active key count" if @expected_active_keys.nil?
        raise UnsafeState, "review deployment SSH target policy before applying" unless @deployment_policy_reviewed

        plans.each { |plan| apply_plan(plan) }
      end
      report.merge(status: apply ? "upgraded" : "ready",
                   deployment_policy_review_required: !@deployment_policy_reviewed)
    end
  rescue UnsafeState
    raise
  rescue StandardError
    # Validation/decryption exceptions can include attribute values. Never
    # render them from a credential conversion task.
    raise UnsafeState, "SSH upgrade failed; inspect counts before retrying", cause: nil
  end

  private

  def plan_for(row, report, lock:)
    if row.revoked_at.present?
      report[:skipped_revoked] += 1
      return
    end
    if row.expires_at && row.expires_at <= Time.current
      report[:skipped_expired] += 1
      return
    end
    users = User.where(id: row.user_id)
    users = users.lock if lock
    user = users.first
    raise UnsafeState, "legacy key has no owner" unless user
    unless user.active?
      report[:skipped_inactive] += 1
      return
    end
    # The upstream model has no expiry field. Do not silently convert a
    # finite-lived credential into an unbounded grant.
    raise UnsafeState, "an active legacy key has an expiry; migrate its lifetime policy explicitly" if row.expires_at

    private_key = SshKey.normalize(row.private_key.to_s)
    public_key = SshKey.public_key(private_key).split.first(2).join(" ")
    unless public_key == row.public_key.to_s.split.first(2).join(" ")
      raise UnsafeState, "legacy public and private key do not match"
    end
    foreign_id = "legacy-ssh-connection-#{row.id}"
    by_owner = SshKeySecret.where(owner_id: user.id)
    by_owner = by_owner.lock if lock
    existing = by_owner.first
    by_marker = SshKeySecret.find_by(foreign_id: foreign_id)
    if by_marker && by_marker != existing
      raise UnsafeState, "legacy migration marker belongs to a different owner"
    end
    principals = expected_principals(user)
    if existing
      verify_existing!(existing, foreign_id, user, public_key, principals)
      report[:already_upgraded] += 1
    else
      candidate = SshKeySecret.new(name: row.name, owner: user, created_by: user,
                                  foreign_id: foreign_id, private_key: private_key)
      raise UnsafeState, "legacy key cannot satisfy upstream key validation" unless candidate.valid?
      report[:create] += 1
    end
    report[:eligible] += 1
    report[:expected_grants] += principals.length
    { row: row, user: user, existing: existing, foreign_id: foreign_id,
      private_key: private_key, public_key: public_key, principals: principals }
  end

  def expected_principals(user)
    scopes = [ Principal.where(kind: "console_user", console_user_id: user.id) ]
    identity = UserIdentity.unambiguous_slack_identity(user.user_identities.slack.order(:id))
    if identity
      subject, team = identity
      scopes << Principal.where(kind: "slack_dm", slack_user_id: subject, slack_team_id: team)
    end
    Principal.combine_scopes(scopes).pluck(:id).sort
  end

  def verify_existing!(key, marker, user, public_key, principal_ids)
    source = key.key_source
    unless key.foreign_id == marker && key.created_by_id == user.id &&
           key.public_key.to_s.split.first(2).join(" ") == public_key &&
           source&.source_type == "control_plane" &&
           SshKey.public_key(source.secret.to_s).split.first(2).join(" ") == public_key
      raise UnsafeState, "owner already has a different or modified upstream key"
    end
    grants = key.grants.pluck(:principal_id, :role_id)
    if grants.any? { |principal, role| role || !principal_ids.include?(principal) }
      raise UnsafeState, "existing migrated key has grants outside its verified owner principals"
    end
  end

  def apply_plan(plan)
    key = plan[:existing] || SshKeySecret.create!(
      name: plan[:row].name, owner: plan[:user], created_by: plan[:user],
      foreign_id: plan[:foreign_id], private_key: plan[:private_key]
    )
    PersonalSshKeyGrants.new.apply_for_user(plan[:user])
    verify_existing!(key, plan[:foreign_id], plan[:user], plan[:public_key], plan[:principals])
    unless key.grants.where(role_id: nil).pluck(:principal_id).sort == plan[:principals]
      raise UnsafeState, "upstream owner grant mapping differs from the reviewed mapping"
    end
  end
end
