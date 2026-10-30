class MakeSshConnectionExpiryOptional < ActiveRecord::Migration[8.1]
  def up
    change_column_null :ssh_connections, :expires_at, true
  end

  def down
    # Old authorizers require an expiry. Expire permanent grants rather than
    # silently extending access when returning to the old application version.
    execute "UPDATE ssh_connections SET expires_at = CURRENT_TIMESTAMP WHERE expires_at IS NULL"
    change_column_null :ssh_connections, :expires_at, false
  end
end
