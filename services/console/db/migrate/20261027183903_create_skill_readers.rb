class CreateSkillReaders < ActiveRecord::Migration[8.1]
  def change
    create_table :skill_readers do |t|
      t.references :skill, null: false, foreign_key: { on_delete: :cascade }
      t.references :user, null: false, foreign_key: { on_delete: :cascade }

      t.timestamps
    end

    add_index :skill_readers, [ :skill_id, :user_id ], unique: true
  end
end
