class MakeSshConnectionsReusableKeys < ActiveRecord::Migration[8.1]
  def change
    change_column_null :ssh_connections, :destination, true
    change_column_null :ssh_connections, :username, true
    change_column_null :ssh_connections, :host_key, true
  end
end
