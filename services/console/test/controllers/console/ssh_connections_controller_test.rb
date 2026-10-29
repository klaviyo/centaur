require "test_helper"
require "tmpdir"
require "open3"


class Console::SshConnectionsControllerTest < ActionDispatch::IntegrationTest
  setup do
    post login_url, params: { email: users(:member_user).email, password: "password123456" }
    @private_key, @public_key = Dir.mktmpdir do |dir|
      path = File.join(dir, "synthetic")
      _out, _err, status = Open3.capture3("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", path)
      raise "synthetic key generation failed" unless status.success?
      [ File.read(path), File.read("#{path}.pub").split.first(2).join(" ") ]
    end
    @attributes = { name: "My EE", destination: "wss://ee.example.com/events/events", username: "dev", host_key: @public_key, private_key: @private_key, duration_hours: 24 }
  end

  test "non-admin can enroll manage renew and disconnect only their own connection" do
    get console_integrations_url
    assert_select "a[href=?]", edit_console_ssh_connection_path, text: "Connect"
    patch console_ssh_connection_url, params: { ssh_connection: @attributes.merge(user_id: users(:acme_admin).id) }
    assert_redirected_to console_ssh_connection_path
    record = users(:member_user).reload.ssh_connection
    assert record
    assert_nil users(:acme_admin).ssh_connection
    get console_ssh_connection_url
    assert_response :ok
    assert_select "textarea#ssh-public-key", text: @public_key
    assert_no_match(/BEGIN OPENSSH PRIVATE KEY/, response.body)
    get edit_console_ssh_connection_url
    assert_response :ok
    assert_select "textarea[name='ssh_connection[private_key]']", text: ""
    patch console_ssh_connection_url, params: { ssh_connection: { name: "Renewed", private_key: "", duration_hours: 168 } }
    assert_redirected_to console_ssh_connection_path
    assert_in_delta 7.days.from_now.to_i, record.reload.expires_at.to_i, 5
    delete console_ssh_connection_url
    assert_redirected_to console_integrations_path
    assert record.reload.revoked?
    assert_nil record.private_key
  end

  test "validation never echoes a submitted private key" do
    patch console_ssh_connection_url, params: { ssh_connection: @attributes.merge(destination: "https://invalid.example.com") }
    assert_response :unprocessable_entity
    assert_select "textarea[name='ssh_connection[private_key]']", text: ""
    assert_select "textarea[name='ssh_connection[private_key]'][required]"
    assert_no_match(/BEGIN OPENSSH PRIVATE KEY/, response.body)
    assert_nil users(:member_user).reload.ssh_connection
  end

  test "other users cannot access a saved connection" do
    patch console_ssh_connection_url, params: { ssh_connection: @attributes }
    delete logout_url
    post login_url, params: { email: users(:globex_admin).email, password: "password123456" }
    get console_ssh_connection_url
    assert_redirected_to edit_console_ssh_connection_path
    delete console_ssh_connection_url
    assert users(:member_user).reload.ssh_connection.configured?
  end

  test "anonymous pending and disabled users cannot configure SSH" do
    delete logout_url
    get edit_console_ssh_connection_url
    assert_redirected_to login_path
    patch console_ssh_connection_url, params: { ssh_connection: @attributes }
    assert_redirected_to login_path
    %i[pending_user disabled_user].each do |fixture|
      post login_url, params: { email: users(fixture).email, password: "password123456" }
      patch console_ssh_connection_url, params: { ssh_connection: @attributes }
      assert_response :redirect
      assert_nil users(fixture).reload.ssh_connection
      delete logout_url
    end
  end
end
