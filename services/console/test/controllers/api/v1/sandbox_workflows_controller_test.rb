require "test_helper"

class Api::V1::SandboxWorkflowsControllerTest < ActionDispatch::IntegrationTest
  class FakeClient
    attr_reader :request

    def list_workflow_schedules
      { "schedules" => [ { "workflow_name" => "scheduled_flow", "input" => { "principal" => "configured-principal" } } ] }
    end

    def create_workflow_run(**request)
      @request = request
      { "run_id" => "run-shared" }
    end
  end

  setup do
    @user = users(:member_user)
    @owner = users(:acme_admin)
    @workflow = WorkflowAccess.create!(name: "scheduled_flow", owner: @owner)
    principal = Principal.create!(foreign_id: "workflow-acl-user", name: "Workflow user", kind: :console_user, console_user: @user, created_by: @user, sandbox_workflows_read_enabled: true, sandbox_workflows_write_enabled: true)
    @proxy = Proxy.create!(name: "workflow-acl-proxy", principal: principal, bearer_token_hash: Digest::SHA256.hexdigest("iprx_#{'f' * 64}"))
    @original_factory = Api::V1::Sandbox::WorkflowsController.client_factory
    @client = FakeClient.new
    Api::V1::Sandbox::WorkflowsController.client_factory = -> { @client }
  end

  teardown do
    Api::V1::Sandbox::WorkflowsController.client_factory = @original_factory
  end

  test "private workflow is absent and cannot be started" do
    with_token do |headers|
      get "/api/v1/sandbox/workflows", headers: headers
      assert_response :ok
      assert_empty JSON.parse(response.body).fetch("data")
      get "/api/v1/sandbox/workflows/#{@workflow.name}", headers: headers
      assert_response :not_found
      post "/api/v1/sandbox/workflows/#{@workflow.name}/run", headers: headers
      assert_response :not_found
    end
    assert_nil @client.request
  end

  test "reader can discover and inspect but cannot start or manage access" do
    @workflow.replace_access!(reader_oids: [ @user.oid ])
    with_token do |headers|
      get "/api/v1/sandbox/workflows", headers: headers
      assert_equal [ @workflow.name ], JSON.parse(response.body).fetch("data").map { |entry| entry["name"] }
      get "/api/v1/sandbox/workflows/#{@workflow.name}", headers: headers
      assert_response :ok
      post "/api/v1/sandbox/workflows/#{@workflow.name}/run", headers: headers
      assert_response :not_found
      patch "/api/v1/sandbox/workflows/#{@workflow.name}/access", params: { data: { editor_oids: [ @user.oid ] } }, headers: headers, as: :json
      assert_response :not_found
    end
    assert_nil @client.request
  end

  test "editor starts with registered input and cannot supply another principal" do
    @workflow.replace_access!(editor_oids: [ @user.oid ])
    with_token do |headers|
      post "/api/v1/sandbox/workflows/#{@workflow.name}/run", params: { principal: "admin", input: { principal: "admin" } }, headers: headers, as: :json
      assert_response :accepted
    end
    assert_equal({ workflow_name: "scheduled_flow", input: { "principal" => "configured-principal" } }, @client.request)
  end

  test "revocation and disabled user take effect immediately" do
    @workflow.replace_access!(reader_oids: [ @user.oid ])
    @workflow.replace_access!(reader_oids: [])
    with_token do |headers|
      get "/api/v1/sandbox/workflows/#{@workflow.name}", headers: headers
      assert_response :not_found
    end
    @user.update!(status: :disabled)
    with_token do |headers|
      get "/api/v1/sandbox/workflows", headers: headers
      assert_response :forbidden
    end
  end

  test "ACL grants cannot bypass disabled sandbox workflow capabilities" do
    @workflow.replace_access!(editor_oids: [ @user.oid ])
    @proxy.principal.update!(sandbox_workflows_read_enabled: false, sandbox_workflows_write_enabled: false)
    with_token do |headers|
      get "/api/v1/sandbox/workflows/#{@workflow.name}", headers: headers
      assert_response :forbidden
      post "/api/v1/sandbox/workflows/#{@workflow.name}/run", headers: headers
      assert_response :forbidden
    end
    assert_nil @client.request
  end

  private

  def with_token
    with_env("CENTAUR_JWT_SIGNING_SECRET" => "test-secret") do
      yield "Authorization" => "Bearer #{SandboxEntitlements::Jwt.encode_for_proxy(@proxy)}"
    end
  end
end
