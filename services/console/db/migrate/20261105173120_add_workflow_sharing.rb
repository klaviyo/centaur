class AddWorkflowSharing < ActiveRecord::Migration[8.1]
  def change
    add_column :scheduled_tasks, :visibility, :string, null: false, default: "private"
    add_check_constraint :scheduled_tasks, "visibility IN ('private', 'shared')", name: "scheduled_tasks_visibility"

    create_table :workflow_accesses do |t|
      t.string :name, null: false
      t.references :owner, null: false, foreign_key: { to_table: :users }
      t.string :visibility, null: false, default: "private"
      t.timestamps
    end
    add_index :workflow_accesses, :name, unique: true
    add_check_constraint :workflow_accesses, "visibility IN ('private', 'shared')", name: "workflow_accesses_visibility"
    add_check_constraint :workflow_accesses, "name <> 'console_workflow'", name: "workflow_accesses_no_task_runner"

    create_table :workflow_grants do |t|
      t.references :resource, polymorphic: true, null: false
      t.references :user, null: false, foreign_key: { on_delete: :cascade }
      t.string :access, null: false
      t.timestamps
    end
    add_index :workflow_grants, %i[resource_type resource_id user_id access], unique: true, name: "index_workflow_grants_unique_access"
    add_check_constraint :workflow_grants, "access IN ('reader', 'editor')", name: "workflow_grants_access"
    add_check_constraint :workflow_grants, "resource_type IN ('ScheduledTask', 'WorkflowAccess')", name: "workflow_grants_resource_type"
  end
end
