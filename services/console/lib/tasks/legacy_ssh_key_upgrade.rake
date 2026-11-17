namespace :legacy_ssh_keys do
  desc "Validate legacy SSH conversion; set APPLY=true and reviewed count/policy to write"
  task upgrade: :environment do
    count = ENV["EXPECTED_ACTIVE_KEYS"]
    abort "EXPECTED_ACTIVE_KEYS must be a nonnegative integer" if count && !count.match?(/\A\d+\z/)
    upgrade = LegacySshKeyUpgrade.new(
      expected_active_keys: count&.to_i,
      deployment_policy_reviewed: ENV["SSH_DEPLOYMENT_POLICY_REVIEWED"] == "true"
    )
    puts JSON.generate(upgrade.call(apply: ENV["APPLY"] == "true"))
  rescue LegacySshKeyUpgrade::UnsafeState => error
    abort error.message
  end
end
