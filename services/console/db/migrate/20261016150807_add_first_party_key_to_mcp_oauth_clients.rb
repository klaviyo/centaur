class AddFirstPartyKeyToMcpOauthClients < ActiveRecord::Migration[8.1]
  def change
    add_column :mcp_oauth_clients, :first_party_key, :string
    add_index :mcp_oauth_clients, :first_party_key, unique: true
  end
end
