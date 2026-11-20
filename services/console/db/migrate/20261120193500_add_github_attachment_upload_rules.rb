class AddGithubAttachmentUploadRules < ActiveRecord::Migration[8.1]
  def up
    execute <<~SQL
      WITH eligible AS (
        SELECT static_secret_id, MAX(position) + 1 AS position
        FROM request_rules
        JOIN static_secrets ON static_secrets.id = request_rules.static_secret_id
        WHERE static_secrets.kind = 'github_token'
        GROUP BY static_secret_id
        HAVING COUNT(*) FILTER (WHERE host = 'api.github.com') = 1
          AND COUNT(*) FILTER (WHERE host = 'github.com') = 1
          AND COUNT(*) FILTER (WHERE host = 'api.githubcopilot.com') <= 1
          AND BOOL_AND(
            host IS NOT NULL AND host IN ('api.github.com', 'github.com', 'api.githubcopilot.com')
            AND cidr IS NULL AND http_methods = '[]'::jsonb AND paths = '[]'::jsonb
          )
      ), added AS (
        INSERT INTO request_rules (static_secret_id, position, host, http_methods, paths, created_at, updated_at)
        SELECT static_secret_id, position, 'uploads.github.com', '["POST"]'::jsonb,
          '["/user-attachments/assets"]'::jsonb, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP
        FROM eligible
        RETURNING static_secret_id
      ), affected_grants AS (
        SELECT grants.principal_id, grants.role_id FROM grants JOIN added USING (static_secret_id)
      )
      UPDATE principals
      SET sync_config_cache_version = sync_config_cache_version + 1
      WHERE id IN (
        SELECT principal_id FROM affected_grants WHERE principal_id IS NOT NULL
        UNION
        SELECT principal_roles.principal_id FROM principal_roles JOIN affected_grants USING (role_id)
      )
    SQL
  end

  def down
    raise ActiveRecord::IrreversibleMigration
  end
end
