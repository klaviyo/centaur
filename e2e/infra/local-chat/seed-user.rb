# Synthetic identity for the dedicated local playground, never a production seed.
raise "Local playground marker is required" unless ENV["CENTAUR_LOCAL_PLAYGROUND"] == "1"

User.transaction do
  user = User.find_or_initialize_by(email: "e2e-user@example.com")
  raise "Refusing to reuse an admin account" if user.persisted? && user.admin?
  raise "Refusing to reactivate a disabled account" if user.persisted? && user.disabled?
  user.assign_attributes(name: "Local DM tester", status: :active, admin: false)
  user.save!

  identity = UserIdentity.find_or_initialize_by(provider: "slack", subject: "UE2EUSER1")
  if identity.persisted? && identity.user_id != user.id
    raise "Fixture Slack identity belongs to another account"
  end
  identity.assign_attributes(user: user, team_id: "TE2E00001", email: user.email, email_verified: true)
  identity.save!

  # A fresh playground has no DM principal until its first message. API identity
  # reconciliation will match that principal to this user's fixture email.
  dm = Principal.find_by(foreign_id: "slack-user-te2e00001-ue2euser1")
  if dm
    raise "Unexpected fixture principal kind" unless dm.kind == "slack_dm"
    if dm.console_user_id && dm.console_user_id != user.id
      raise "Fixture DM belongs to another account"
    end
    dm.update!(console_user: user, slack_email: user.email)
  end

  execution_principal = ConsoleUserPrincipalProvisioner.call(user)
  [dm, execution_principal].compact.each do |principal|
    if principal.sandbox_workflows_read_enabled || principal.sandbox_workflows_write_enabled
      raise "Local task delegation does not require broad workflow permissions"
    end
  end
  puts({ fixture_user: user.oid, admin: user.admin?, dm_destination: SlackDeliveryPolicy.new(user).direct_message_user_id }.to_json)
end
