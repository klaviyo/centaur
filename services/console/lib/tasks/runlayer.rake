namespace :runlayer do
  desc "Validate legacy Runlayer upgrade; APPLY=true writes metadata (credential writers must be stopped)"
  task migrate_legacy: :environment do
    begin
      result = Oauth::MigrateLegacyRunlayer.new(resource_url: ENV.fetch("RESOURCE_URL", "")).call(apply: ENV["APPLY"] == "true")
      puts JSON.generate(result)
    rescue Oauth::MigrateLegacyRunlayer::UnsafeState => error
      abort error.message
    end
  end
end
