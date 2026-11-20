require "test_helper"
require Rails.root.join("db/migrate/20261120193500_add_github_attachment_upload_rules")

class AddGithubAttachmentUploadRulesTest < ActiveSupport::TestCase
  test "adds one scoped upload rule and invalidates the granted principal" do
    secret = StaticSecret.new(kind: "github_token", name: "Existing GitHub token", created_by: users(:acme_admin))
    rules = %w[api.github.com github.com].map.with_index do |host, position|
      RequestRule.new(host: host, position: position)
    end
    secret.rules = secret.apply_kind_defaults(rules: rules)
    secret.save!
    principal = principals(:acme_channel)
    Grant.create!(principal: principal, static_secret: secret, created_by: users(:acme_admin))
    custom = static_secrets(:github_token_inject)
    original_custom_rules = custom.rules.pluck(:id)

    assert_difference -> { principal.reload.sync_config_cache_version }, 1 do
      AddGithubAttachmentUploadRules.new.up
    end
    assert_equal({ "host" => "uploads.github.com", "methods" => [ "POST" ], "paths" => [ "/user-attachments/assets" ] },
                 secret.reload.rules.last.to_proxy_rule)
    assert_equal original_custom_rules, custom.reload.rules.pluck(:id)
    assert_no_difference -> { secret.rules.reload.count } do
      AddGithubAttachmentUploadRules.new.up
    end
  end
end
