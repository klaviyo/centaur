module Oauth
  # Explicit, offline upgrade of the fork's personal Runlayer app. Run after
  # schema migrations with old Console/refresh workers and credential writes
  # stopped. Never registers a new client, exchanges tokens, or decrypts secrets.
  class MigrateLegacyRunlayer
    class UnsafeState < StandardError; end

    def initialize(resource_url:)
      @setup = Presets::Runlayer.new.setup_from("resource_url" => resource_url)
      raise UnsafeState, "Supply the original exact HTTPS MCP resource URL" unless @setup
    end

    # Dry-run is the default; all checks run before any persisted change.
    # Counts/status are safe to print. Model records and credentials are not.
    def call(apply: false)
      OauthApp.transaction do
        OauthApp.connection.execute("SET LOCAL lock_timeout = '5s'")
        apps = OauthApp.where(provider: "runlayer").or(OauthApp.where(slug: "runlayer")).lock.to_a
        return { status: "absent", credentials: 0, grants: 0 } if apps.empty?
        reject_unless(apps.size == 1, "Expected exactly one Runlayer app")
        app = apps.sole
        config = @setup.fetch(:provider_config)
        reject_unless(app.slug == "runlayer" && !app.shared?, "Runlayer app must have its original personal slug")
        if app.provider == "custom"
          reject_unless(app.owner_only? && app.always_available? && app.provider_config == config &&
            app.labels["centaur-tool"] == "runlayer", "Existing custom app does not match the requested upgrade")
          return { status: "already_upgraded", credentials: app.broker_credentials.count, grants: nil }
        end
        reject_unless(app.provider == "runlayer" && !app.owner_only? && app.provider_config.blank?,
          "Unexpected legacy app configuration")
        reject_unless(app.labels["centaur-tool"].nil? || app.labels["centaur-tool"] == "runlayer",
          "App tool label conflicts with Runlayer")
        reject_unless(app.allowed_scopes == [ "mcp:proxy" ], "Unexpected app scopes; review and reconnect manually")
        credentials = app.broker_credentials.lock.to_a
        grants = credentials.sum { |credential| validate_credential!(credential, config) }
        if apply
          # owner_only is immutable through ordinary APIs. This guarded upgrade
          # changes metadata only, preserving encrypted columns and every ID.
          app.update_columns(provider: "custom", owner_only: true, always_available: true,
            provider_config: config, labels: app.labels.merge("centaur-tool" => "runlayer"), updated_at: Time.current)
        end
        { status: apply ? "upgraded" : "ready", credentials: credentials.size, grants: grants }
      end
    rescue ActiveRecord::LockWaitTimeout
      raise UnsafeState, "Credential writers are still active; stop them and retry. No changes made."
    end

    private

    def reject_unless(condition, message)
      raise UnsafeState, "#{message}; no changes made. Review metadata or reconnect manually." unless condition
    end

    def validate_credential!(credential, config)
      owner = credential.created_by
      reject_unless(owner && credential.provider_subject == "owner-#{owner.oid}" && credential.provider_email.blank?,
        "Credential owner identity is ambiguous")
      reject_unless(credential.labels["runlayer_resource"] == config["resource"] &&
        credential.token_endpoint == config["token_endpoint"] && credential.scopes.include?("mcp:proxy"),
        "Credential resource, endpoint, or scopes do not match")
      wrapper = credential.static_secret
      reject_unless(wrapper, "Credential has no wrapper")
      wrapper.lock!
      source = wrapper.source
      source&.lock!
      reject_unless(source&.source_type == "token_broker" && source.broker_credential_id == credential.id && source.config == { "credential_id" => credential.oid } &&
        wrapper.labels["centaur-tool"] == "runlayer", "Wrapper source or tool label does not match")
      rules = wrapper.rules.lock.to_a
      resource = URI.parse(config.fetch("resource"))
      reject_unless(rules.size == 1 && rules.sole.host == resource.host && rules.sole.cidr.blank? &&
        rules.sole.paths == [ resource.path ] && rules.sole.http_methods.sort == %w[DELETE GET POST],
        "Wrapper request policy differs from the original MCP restriction")
      grants = wrapper.grants.lock.to_a
      reconciliation = PrincipalCredentialReconciliation.new
      reject_unless(grants.all? { |grant| grant.role_id.nil? && reconciliation.connection_owner?(grant.principal, credential) },
        "Wrapper has grants outside its active owner")
      grants.size
    end
  end
end
