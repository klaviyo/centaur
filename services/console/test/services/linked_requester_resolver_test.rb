require "test_helper"

class LinkedRequesterResolverTest < ActiveSupport::TestCase
  setup do
    @app = oauth_apps(:acme_linear)
    @app.update!(client_secret: "linear-secret")
    @user = users(:member_user)
  end

  test "resolves the active user who linked the Linear account" do
    create_credential(@app, "lin-user-1", created_by: @user)

    assert_equal @user, resolve("lin-user-1")
  end

  test "matches the provider account id, never the email" do
    create_credential(@app, "lin-user-1", email: users(:acme_admin).email, created_by: @user)

    assert_nil resolve(users(:acme_admin).email)
    assert_equal @user, resolve("lin-user-1")
  end

  test "ignores unlinked accounts, other providers, and unsupported providers" do
    create_credential(oauth_apps(:acme_github), "lin-user-1", created_by: @user)

    assert_nil resolve("lin-user-1")
    assert_nil resolve("lin-unknown")
    assert_nil LinkedRequesterResolver.call(provider: "github", subject: "lin-user-1")
    assert_nil resolve("")
  end

  test "ignores dead credentials and inactive or missing users" do
    create_credential(@app, "lin-dead", created_by: @user).update!(dead: true)
    create_credential(@app, "lin-disabled", created_by: users(:disabled_user))
    create_credential(@app, "lin-pending", created_by: users(:pending_user))
    create_credential(@app, "lin-orphan", created_by: nil)

    %w[lin-dead lin-disabled lin-pending lin-orphan].each { |subject| assert_nil resolve(subject), subject }
  end

  test "ignores disabled, shared, and owner-only apps" do
    disabled = linear_app("linear-disabled", enabled: false)
    shared = linear_app("linear-shared", shared: true)
    owner_only = linear_app("linear-owner-only", owner_only: true)
    create_credential(disabled, "lin-disabled-app", created_by: @user)
    create_credential(shared, "lin-shared-app", created_by: @user)
    create_credential(owner_only, "lin-owner-only-app", created_by: @user)

    %w[lin-disabled-app lin-shared-app lin-owner-only-app].each { |subject| assert_nil resolve(subject), subject }
  end

  test "an account two different users linked resolves to no one" do
    create_credential(@app, "lin-user-1", created_by: @user)
    create_credential(linear_app("linear-second"), "lin-user-1", created_by: users(:acme_admin))

    assert_nil resolve("lin-user-1")
  end

  test "the same user linking one account through two apps still resolves" do
    create_credential(@app, "lin-user-1", created_by: @user)
    create_credential(linear_app("linear-second"), "lin-user-1", created_by: @user)

    assert_equal @user, resolve("lin-user-1")
  end

  private

  def resolve(subject)
    LinkedRequesterResolver.call(provider: "linear", subject: subject)
  end

  def linear_app(slug, **attrs)
    app = @app.dup
    app.update!(slug: slug, client_secret: "linear-secret", **attrs)
    app
  end

  def create_credential(app, subject, email: nil, created_by:)
    BrokerCredential.create!(
      oauth_app: app,
      provider_subject: subject,
      provider_email: email,
      token_endpoint: app.provider_strategy.token_endpoint,
      refresh_token: "refresh-#{subject}",
      access_token: "access-#{subject}",
      expires_at: 1.hour.from_now,
      last_refresh: Time.current,
      external_user_key: "user-#{subject}",
      created_by: created_by
    )
  end
end
