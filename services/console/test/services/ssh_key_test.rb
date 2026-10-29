require "test_helper"
require "tmpdir"
require "open3"

class SshKeyTest < ActiveSupport::TestCase
  test "reports a passphrase-protected key without exposing parser input" do
    Dir.mktmpdir do |dir|
      path = File.join(dir, "synthetic")
      _out, _err, status = Open3.capture3("ssh-keygen", "-q", "-t", "ed25519", "-N", "test-only-passphrase", "-f", path)
      assert status.success?
      error = assert_raises(SshKey::Invalid) { SshKey.public_key(File.read(path)) }
      assert_includes error.message, "protected by a passphrase"
      assert_not_includes error.message, "test-only-passphrase"
      assert_not_includes error.message, "BEGIN OPENSSH"
    end
  end

  test "reports missing headers incomplete input and malformed key contents" do
    {
      "" => "Paste your private key",
      "copied file contents without headers" => "Include the complete private key",
      "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA" => "key is incomplete",
      "-----BEGIN OPENSSH PRIVATE KEY-----\nAAAA\n-----END OPENSSH PRIVATE KEY-----" => "could not read this key"
    }.each do |value, message|
      error = assert_raises(SshKey::Invalid) { SshKey.public_key(value) }
      assert_includes error.message, message
      assert_not_includes error.message, "AAAA"
    end
  end
end
