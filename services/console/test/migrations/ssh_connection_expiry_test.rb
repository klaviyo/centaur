require "test_helper"
require Rails.root.join("db/migrate/20261029221757_make_ssh_connection_expiry_optional")

class SshConnectionExpiryTest < ActiveSupport::TestCase
  test "rollback expires permanent grants and preserves existing deadlines" do
    expiry = 1.day.from_now.change(usec: 0)
    records = SshConnection.insert_all!([
      { user_id: users(:member_user).id, name: "Permanent", public_key: "synthetic", generation: "test", expires_at: nil },
      { user_id: users(:acme_admin).id, name: "Timed", public_key: "synthetic", generation: "test", expires_at: expiry }
    ])
    permanent, timed = records.rows.flatten
    migration = MakeSshConnectionExpiryOptional.new
    before = SshConnection.connection.select_value("SELECT CURRENT_TIMESTAMP").to_time
    migration.suppress_messages { migration.down }
    assert SshConnection.find(permanent).expires_at <= Time.current
    assert_in_delta before.to_f, SshConnection.find(permanent).expires_at.to_f, 0.001
    assert_equal expiry.to_i, SshConnection.find(timed).expires_at.to_i
    assert_not SshConnection.find(permanent).configured?
    migration.suppress_messages { migration.up }
    SshConnection.find(permanent).update_columns(expires_at: nil)
    assert_nil SshConnection.find(permanent).expires_at
  end
end
