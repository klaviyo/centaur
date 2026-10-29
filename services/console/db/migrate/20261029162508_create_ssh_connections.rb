class CreateSshConnections < ActiveRecord::Migration[8.1]
  def change
    create_table :ssh_connections do |t|
      t.references :user, null: false, foreign_key: true, index: { unique: true }
      t.string :name, null: false
      t.string :destination, null: false
      t.string :username, null: false
      t.text :host_key, null: false
      t.text :public_key, null: false
      t.text :private_key
      t.string :generation, null: false
      t.datetime :expires_at, null: false
      t.datetime :revoked_at

      t.timestamps
    end
  end
end
