require "test_helper"

class LegacySshKeyUpgradeTest < ActiveSupport::TestCase
  setup do
    assert LegacySshKeyUpgrade::LegacyConnection.table_exists?, "retained fork schema is required"
    @owner = users(:member_user)
    @private_key, @public_key = synthetic_ssh_key
  end

  def legacy(user: @owner, **attributes)
    LegacySshKeyUpgrade::LegacyConnection.create!({ user_id: user.id, name: "Synthetic key",
      private_key: @private_key, public_key: @public_key, generation: SecureRandom.uuid }.merge(attributes))
  end

  def upgrade(expected: 1, reviewed: true)
    LegacySshKeyUpgrade.new(expected_active_keys: expected, deployment_policy_reviewed: reviewed)
  end

  test "default dry run validates without writing or exposing keys" do
    row = legacy
    original = row.attributes
    assert_no_difference [ "SshKeySecret.count", "SecretSource.count", "Grant.count" ] do
      result = upgrade.call
      assert_equal "dry_run", result[:mode]
      assert_equal 1, result[:create]
      assert_not_includes result.to_json, @private_key
      assert_not_includes result.to_json, @public_key
    end
    assert_equal original, row.reload.attributes
  end

  test "apply preserves legacy rows and grants only verified owner principals" do
    row = legacy
    console = Principal.create!(foreign_id: "upgrade-console", kind: "console_user", console_user: @owner, created_by: @owner)
    @owner.user_identities.create!(provider: "slack", subject: "U12345678", team_id: "T12345678")
    dm = Principal.create!(foreign_id: "upgrade-dm", kind: "slack_dm", slack_user_id: "U12345678", slack_team_id: "T12345678", created_by: @owner)
    channel = Principal.create!(foreign_id: "upgrade-channel", kind: "slack_channel", slack_user_id: "U12345678", slack_team_id: "T12345678", created_by: @owner)
    other = Principal.create!(foreign_id: "upgrade-other", kind: "console_user", console_user: users(:acme_admin), created_by: users(:acme_admin))
    original = row.attributes

    assert_difference "SshKeySecret.count", 1 do
      assert_equal "upgraded", upgrade.call(apply: true)[:status]
    end
    key = @owner.reload.personal_ssh_key
    assert_equal "legacy-ssh-connection-#{row.id}", key.foreign_id
    assert_equal @public_key, key.public_key
    assert_equal @private_key.strip, key.key_source.secret.strip
    assert_equal [ console.id, dm.id ].sort, key.grants.pluck(:principal_id).sort
    assert_not key.grants.exists?(principal: channel)
    assert_not key.grants.exists?(principal: other)
    assert_equal original, row.reload.attributes
    ciphertext = SecretSource.connection.select_value("SELECT secret FROM secret_sources WHERE id = #{key.key_source.id.to_i}")
    assert_not_includes ciphertext, @private_key.strip
  end

  test "repeated apply verifies the same key and does not duplicate grants" do
    legacy
    upgrade.call(apply: true)
    assert_no_difference [ "SshKeySecret.count", "SecretSource.count", "Grant.count" ] do
      assert_equal 1, upgrade.call(apply: true)[:already_upgraded]
    end
  end

  test "revoked expired and inactive owners never become active upstream keys" do
    legacy(revoked_at: Time.current)
    legacy(user: users(:acme_admin), expires_at: 1.hour.ago)
    legacy(user: users(:disabled_user))
    assert_no_difference "SshKeySecret.count" do
      result = upgrade(expected: 0).call(apply: true)
      assert_equal 1, result[:skipped_revoked]
      assert_equal 1, result[:skipped_expired]
      assert_equal 1, result[:skipped_inactive]
    end
  end

  test "a finite future expiry cannot silently become an unbounded key" do
    legacy(expires_at: 1.hour.from_now)
    assert_raises(LegacySshKeyUpgrade::UnsafeState) { upgrade.call }
    assert_no_difference "SshKeySecret.count" do
      assert_raises(LegacySshKeyUpgrade::UnsafeState) { upgrade.call(apply: true) }
    end
  end

  test "apply requires the explicit reviewed count and deployment policy" do
    legacy
    [ upgrade(expected: nil), upgrade(expected: 2), upgrade(reviewed: false) ].each do |tool|
      assert_no_difference "SshKeySecret.count" do
        assert_raises(LegacySshKeyUpgrade::UnsafeState) { tool.call(apply: true) }
      end
    end
  end

  test "conflicting target ownership or altered migrated key aborts" do
    legacy
    @owner.create_personal_ssh_key!(name: "Existing key", created_by: @owner, private_key: @private_key)
    assert_raises(LegacySshKeyUpgrade::UnsafeState) { upgrade.call }
  end

  test "the complete batch is checked before any conversion" do
    legacy
    legacy(user: users(:acme_admin), public_key: "invalid public key")
    assert_no_difference [ "SshKeySecret.count", "SecretSource.count", "Grant.count" ] do
      assert_raises(LegacySshKeyUpgrade::UnsafeState) { upgrade(expected: 2).call(apply: true) }
    end
  end

  test "a grant mismatch during apply rolls back every new credential" do
    legacy
    PersonalSshKeyGrants.stub(:new, Object.new.tap { |object| def object.apply_for_user(_); end }) do
      Principal.create!(foreign_id: "upgrade-console", kind: "console_user", console_user: @owner, created_by: @owner)
      assert_no_difference [ "SshKeySecret.count", "SecretSource.count", "Grant.count" ] do
        assert_raises(LegacySshKeyUpgrade::UnsafeState) { upgrade.call(apply: true) }
      end
    end
  end

  test "decryption errors are sanitized" do
    row = legacy
    row.update_column(:private_key, "not an encrypted key")
    error = assert_raises(LegacySshKeyUpgrade::UnsafeState) { upgrade.call }
    assert_equal "SSH upgrade failed; inspect counts before retrying", error.message
    assert_nil error.cause
  end
end
