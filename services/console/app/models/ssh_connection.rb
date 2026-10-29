require "uri"

class SshConnection < ApplicationRecord
  belongs_to :user
  encrypts :private_key
  attr_accessor :duration_hours

  DURATIONS = [ 24, 168, 720 ].freeze
  normalizes :name, :destination, :username, :host_key, with: ->(value) { value.to_s.strip }
  before_validation :prepare_connection
  validates :user_id, uniqueness: true
  validates :name, presence: true, length: { maximum: 100 }
  validates :username, format: { with: /\A[a-zA-Z0-9_][a-zA-Z0-9_.-]{0,63}\z/ }, allow_nil: true
  validates :destination, length: { maximum: 2048 }
  validates :host_key, length: { maximum: 16.kilobytes }
  validates :expires_at, :generation, presence: true
  validate :valid_destination, if: -> { destination.present? }
  validate :valid_host_key, if: -> { host_key.present? }
  validate :valid_private_key, unless: :revoked?

  def configured? = !revoked? && expires_at&.future?
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
    return if duration_hours.nil?

    if DURATIONS.map(&:to_s).include?(duration_hours.to_s)
      self.expires_at = duration_hours.to_i.hours.from_now
    else
      errors.add(:duration_hours, "must be 24 hours, 7 days, or 30 days")
    end
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
  rescue SshKey::Invalid
    errors.add(:private_key, "must be a valid private key without a passphrase")
  end
end
