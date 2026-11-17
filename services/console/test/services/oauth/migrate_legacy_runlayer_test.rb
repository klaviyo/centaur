require "test_helper"

class Oauth::MigrateLegacyRunlayerTest < ActiveSupport::TestCase
  RESOURCE = "https://tenant.example.test/mcp".freeze

  setup do
    @owner = users(:member_user)
    @principal = Principal.create!(kind: "console_user", console_user: @owner, created_by: @owner, name: "Runlayer owner")
    config = Oauth::Presets::Runlayer.new.setup_from("resource_url" => RESOURCE).fetch(:provider_config)
    @app = OauthApp.create!(slug: "runlayer", provider: "custom", provider_config: config,
      owner_only: true, always_available: true, labels: { "centaur-tool" => "runlayer" },
      client_id: "synthetic-client", client_secret: "synthetic-secret", allowed_scopes: [ "mcp:proxy" ], created_by: @owner)
    @credential = BrokerCredential.create!(oauth_app: @app, created_by: @owner, provider_subject: "owner-#{@owner.oid}",
      name: "Synthetic Runlayer", token_endpoint: config.fetch("token_endpoint"), scopes: [ "mcp:proxy" ],
      labels: { "runlayer_resource" => RESOURCE }, refresh_token: "synthetic-refresh")
    @wrapper = StaticSecret.create!(name: "Synthetic Runlayer wrapper", broker_credential: @credential,
      created_by: @owner, labels: { "centaur-tool" => "runlayer" }, inject_config: { "header" => "Authorization", "formatter" => "Bearer {{ .Value }}" })
    @source = SecretSource.create!(static_secret: @wrapper, source_type: "token_broker", config: { "credential_id" => @credential.oid })
    @rule = @wrapper.rules.create!(position: 0, host: "tenant.example.test", paths: [ "/mcp" ], http_methods: %w[GET POST DELETE])
    @grant = Grant.find_or_create_by!(principal: @principal, static_secret: @wrapper) { |grant| grant.created_by = @owner }
    # Model APIs intentionally cannot create legacy providers or toggle owner_only.
    @app.update_columns(provider: "runlayer", owner_only: false, provider_config: {})
  end

  test "dry run preserves all records and apply changes metadata only and is idempotent" do
    before = snapshots
    assert_equal({ status: "ready", credentials: 1, grants: 1 }, upgrade.call)
    assert_equal before, snapshots
    assert_equal({ status: "upgraded", credentials: 1, grants: 1 }, upgrade.call(apply: true))
    after = snapshots
    before.each_key do |key|
      next if key == :app
      assert_equal before[key], after[key], key.to_s
    end
    %w[id client_id client_secret created_by_id].each do |column|
      assert_equal before[:app][column], after[:app][column], column
    end
    assert_equal "custom", @app.reload.provider
    assert @app.owner_only?
    assert_equal RESOURCE, @app.provider_config.fetch("resource")
    assert_equal "already_upgraded", upgrade.call(apply: true).fetch(:status)
  end

  test "mismatched resource or endpoint aborts without writes" do
    @credential.update_columns(token_endpoint: "https://other.example.test/api/v1/oauth/token")
    assert_rejected
  end

  test "missing original resource metadata aborts without writes" do
    @credential.update_columns(labels: {})
    assert_rejected
  end

  test "ambiguous owner identity aborts without writes" do
    @credential.update_columns(provider_subject: "someone-else")
    assert_rejected
  end

  test "cross-owner direct grants abort without writes" do
    Grant.where(id: @grant.id).update_all(principal_id: principals(:acme_channel).id)
    assert_rejected
  end

  test "role grants abort without writes" do
    Grant.where(id: @grant.id).update_all(principal_id: nil, role_id: roles(:acme_infra).id)
    assert_rejected
  end

  test "a substituted wrapper source aborts without writes" do
    @source.update_columns(config: { "credential_id" => broker_credentials(:acme_managed_gmail).oid })
    assert_rejected
  end

  test "broadened method policy aborts without writes" do
    @rule.update_columns(http_methods: [])
    assert_rejected
  end

  test "two legacy apps or a conflicting modern app abort without writes" do
    extra = OauthApp.create!(slug: "other-runlayer", provider: "google", client_id: "synthetic",
      client_secret: "synthetic", allowed_scopes: [ "openid" ], created_by: @owner)
    extra.update_columns(provider: "runlayer")
    assert_rejected
  end

  test "invalid URL is rejected before database mutation" do
    assert_raises(Oauth::MigrateLegacyRunlayer::UnsafeState) do
      Oauth::MigrateLegacyRunlayer.new(resource_url: "http://tenant.example.test/mcp")
    end
  end

  test "Runlayer custom provider keeps exact MCP methods and path" do
    upgrade.call(apply: true)
    assert_equal [ { host: "tenant.example.test", paths: [ "/mcp" ], http_methods: %w[GET POST DELETE] } ],
      @app.reload.provider_strategy.credential_request_rules
    @app.update!(labels: {})
    assert_equal [], @app.provider_strategy.credential_request_rules.sole.fetch(:http_methods)
  end

  private

  def upgrade
    Oauth::MigrateLegacyRunlayer.new(resource_url: RESOURCE)
  end

  def snapshots
    { app: @app.reload.attributes_before_type_cast, credential: @credential.reload.attributes_before_type_cast,
      wrapper: @wrapper.reload.attributes_before_type_cast, source: @source.reload.attributes_before_type_cast,
      rule: @rule.reload.attributes_before_type_cast, grant: @grant.reload.attributes_before_type_cast }
  end

  def assert_rejected
    before = snapshots
    assert_raises(Oauth::MigrateLegacyRunlayer::UnsafeState) { upgrade.call(apply: true) }
    assert_equal before, snapshots
  end
end
