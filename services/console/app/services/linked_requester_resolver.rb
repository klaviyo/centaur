# Resolves the Console user who linked a provider account through Console's
# OAuth consent flow, so a turn that provider's ingress starts can run as the
# person who triggered it. The consent itself is the proof: the user was signed
# in to Console and authorized that provider account, whose id the provider
# strategy stored as the credential's provider_subject (for Linear, the Linear
# user id).
#
# Only connections that identify a provider account count: the app must be
# enabled, not shared (an admin-connected account the whole org uses), and not
# owner_only (keyed by the Console user rather than the provider account). The
# credential must be live and its connecting user active. Emails are never
# matched, since a provider email can drift from the Console login. When
# different active users linked the same account through different apps, the
# account resolves to no one.
class LinkedRequesterResolver
  PROVIDERS = %w[linear].freeze

  def self.call(provider:, subject:)
    new(provider: provider, subject: subject).call
  end

  def initialize(provider:, subject:)
    @provider = provider
    @subject = subject
  end

  # The linked, active Console user, or nil.
  def call
    return unless PROVIDERS.include?(provider) && subject.present?

    users = User.active.where(id: linked_credentials.select(:created_by_id)).limit(2).to_a
    users.first if users.one?
  end

  private

  attr_reader :provider, :subject

  def linked_credentials
    BrokerCredential
      .joins(:oauth_app)
      .where(provider_subject: subject, dead: false)
      .where(oauth_apps: { provider: provider, enabled: true, shared: false, owner_only: false })
  end
end
