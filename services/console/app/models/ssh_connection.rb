require "uri"

class SshConnection < ApplicationRecord
  belongs_to :user
  encrypts :private_key

  normalizes :name, :destination, :username, :host_key, with: ->(value) { value.to_s.strip }
  before_validation :prepare_connection
  validates :user_id, uniqueness: true
  validates :name, presence: true, length: { maximum: 100 }
  validates :username, format: { with: /\A[a-zA-Z0-9_][a-zA-Z0-9_.-]{0,63}\z/ }, allow_nil: true
  validates :destination, length: { maximum: 2048 }
  validates :host_key, length: { maximum: 16.kilobytes }
  validates :generation, presence: true
  validate :valid_destination, if: -> { destination.present? }
  validate :valid_host_key, if: -> { host_key.present? }
  validate :valid_private_key, unless: :revoked?

  def configured? = !revoked? && (expires_at.nil? || expires_at.future?)
  def revoked? = revoked_at.present?

  def revoke!
    # Keep a tombstone so disconnect cannot reactivate a legacy file grant.
    update!(revoked_at: Time.current, private_key: nil, generation: SecureRandom.uuid)
  end

  def key_fingerprint
    SshKey.fingerprint(public_key)
  rescue SshKey::Invalid
    nil
  end

  private

  def prepare_connection
    self.generation = SecureRandom.uuid if new_record? || changed?
  end

  def valid_destination
    uri = URI.parse(destination.to_s)
    valid = uri.is_a?(URI::WSS) && uri.host.present? && uri.port == 443 &&
      uri.userinfo.nil? && uri.query.nil? && uri.fragment.nil? && uri.path.start_with?("/")
    errors.add(:destination, "must be a wss:// SSH tunnel URL on port 443 without credentials, query, or fragment") unless valid
  rescue URI::InvalidURIError
    errors.add(:destination, "must be a valid wss:// SSH tunnel URL")
  end

  def valid_host_key
    SshKey.fingerprint(host_key)
  rescue SshKey::Invalid
    errors.add(:host_key, "must be a verified SSH public host key, starting with ssh-ed25519, ssh-rsa, or ecdsa-sha2")
  end

  def valid_private_key
    return if persisted? && !will_save_change_to_private_key? && private_key.present?

    self.public_key = SshKey.public_key(private_key)
    self.private_key = SshKey.normalize(private_key)
  rescue SshKey::Invalid => error
    errors.add(:private_key, error.message)
  end
end
