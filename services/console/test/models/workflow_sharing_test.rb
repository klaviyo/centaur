require "test_helper"

class WorkflowSharingTest < ActiveSupport::TestCase
  setup do
    @owner = users(:acme_admin)
    @reader = users(:member_user)
    @editor = users(:globex_admin)
    @workflow = WorkflowAccess.create!(name: "shared_workflow", owner: @owner)
  end

  test "private default and independent reader editor permissions" do
    assert @workflow.readable_by?(@owner)
    assert_not @workflow.readable_by?(@reader)
    @workflow.replace_access!(reader_oids: [ @reader.oid ], editor_oids: [ @editor.oid ])
    assert @workflow.readable_by?(@reader)
    assert_not @workflow.editable_by?(@reader)
    assert @workflow.editable_by?(@editor)
    assert_includes WorkflowAccess.readable_by(@reader), @workflow
    assert_not_includes WorkflowAccess.editable_by(@reader), @workflow
    assert_includes WorkflowAccess.editable_by(@editor), @workflow
  end

  test "revocation is immediate and invalid changes roll back" do
    @workflow.replace_access!(reader_oids: [ @reader.oid ])
    assert_raises(ActiveRecord::RecordInvalid) do
      @workflow.replace_access!(reader_oids: [], editor_oids: [ "usr_invalid" ])
    end
    assert @workflow.readable_by?(@reader)
    @workflow.replace_access!(reader_oids: [])
    assert_not @workflow.readable_by?(@reader)
  end

  test "shared grants read only and disabled users lose access" do
    @workflow.replace_access!(visibility: "shared", editor_oids: [ @reader.oid ])
    assert @workflow.readable_by?(@editor)
    assert_not @workflow.editable_by?(@editor)
    @reader.update!(status: :disabled)
    assert_not @workflow.readable_by?(@reader)
    assert_empty WorkflowAccess.readable_by(@reader)
    assert_empty WorkflowAccess.editable_by(@reader)
  end

  test "cannot grant owner disabled or nonexistent users" do
    assert_raises(ActiveRecord::RecordInvalid) { @workflow.replace_access!(reader_oids: [ @owner.oid ]) }
    @reader.update!(status: :disabled)
    assert_raises(ActiveRecord::RecordInvalid) { @workflow.replace_access!(editor_oids: [ @reader.oid ]) }
  end

  test "global scheduled task runner cannot be shared" do
    assert_not WorkflowAccess.new(name: ScheduledTask::WORKFLOW_NAME, owner: @owner).valid?
  end

  test "access can be revoked when task delivery becomes invalid" do
    task = ScheduledTask.create!(name: "Revocable task", author: @owner, prompt: "Summarize", delivery_channel: "C0123456789")
    task.replace_access!(reader_oids: [ @reader.oid ])
    task.update_columns(delivery_channel: "unavailable")
    assert_not task.valid?
    task.replace_access!(reader_oids: [])
    assert_not task.readable_by?(@reader)
  end

  test "grant is destroyed with its workflow" do
    @workflow.replace_access!(reader_oids: [ @reader.oid ])
    assert_difference("WorkflowGrant.count", -1) { @workflow.destroy! }
  end
end
