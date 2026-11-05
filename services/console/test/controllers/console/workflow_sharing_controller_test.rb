require "test_helper"

class Console::WorkflowSharingControllerTest < ActionDispatch::IntegrationTest
  setup do
    @owner = users(:acme_admin)
    @reader = users(:member_user)
    @editor = users(:globex_admin)
    @workflow = WorkflowAccess.create!(name: "shared_flow", owner: @owner)
    @workflow.replace_access!(reader_oids: [ @reader.oid ], editor_oids: [ @editor.oid ])
  end

  test "owner manages access and readers cannot grant themselves edit" do
    sign_in(@owner)
    patch access_console_workflow_url(@workflow.name), params: { access: { visibility: "shared" } }
    assert_redirected_to console_workflow_url(@workflow.name)
    assert @workflow.reload.visibility_shared?
    sign_in(@reader)
    patch access_console_workflow_url(@workflow.name), params: { access: { editor_oids: [ @reader.oid ] } }
    assert_response :not_found
    assert_not @workflow.editable_by?(@reader)
  end

  test "reader cannot trigger and revoked reader cannot inspect" do
    sign_in(@reader)
    post run_console_workflow_url(@workflow.name)
    assert_response :not_found
    @workflow.replace_access!(reader_oids: [])
    get console_workflow_url(@workflow.name)
    assert_response :not_found
  end

  test "unconfigured workflows stay admin only" do
    sign_in(@reader)
    get console_workflow_url("unconfigured")
    assert_response :not_found
    patch access_console_workflow_url("unconfigured"), params: { access: { owner_oid: @reader.oid } }
    assert_response :not_found
  end

  test "admin can recover access only when the workflow owner is inactive" do
    sign_in(@editor)
    patch access_console_workflow_url(@workflow.name), params: { access: { reader_oids: [] } }
    assert_response :not_found
    assert @workflow.workflow_grants.exists?(user: @reader, access: "reader")

    @owner.update!(status: :disabled)
    patch access_console_workflow_url(@workflow.name), params: {
      access: { visibility: "shared", reader_oids: [], editor_oids: [ @reader.oid ], owner_oid: @editor.oid }
    }
    assert_redirected_to console_workflow_url(@workflow.name)
    assert @workflow.reload.visibility_shared?
    assert_equal @owner, @workflow.owner
    assert_not @workflow.workflow_grants.exists?(access: "reader")
    assert @workflow.editable_by?(@reader)
    assert_not @workflow.editable_by?(@editor)
  end

  test "non-admin editor cannot recover access for an inactive workflow owner" do
    @workflow.replace_access!(reader_oids: [], editor_oids: [ @reader.oid ])
    @owner.update!(status: :disabled)
    sign_in(@reader)

    patch access_console_workflow_url(@workflow.name), params: { access: { visibility: "shared" } }

    assert_response :not_found
    assert @workflow.reload.visibility_private?
    assert_equal @owner, @workflow.owner
  end

  private

  def sign_in(user)
    delete logout_url
    post login_url, params: { email: user.email, password: "password123456" }
  end
end
