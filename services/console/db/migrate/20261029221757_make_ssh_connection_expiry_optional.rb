class MakeSshConnectionExpiryOptional < ActiveRecord::Migration[8.1]
  def change
    change_column_null :ssh_connections, :expires_at, true
  end
end
