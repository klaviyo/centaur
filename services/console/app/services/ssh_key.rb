require "open3"
require "base64"
require "digest"

class SshKey
  class Invalid < StandardError; end

  def self.public_key(private_key)
    raise Invalid if private_key.blank? || private_key.bytesize > 64.kilobytes

    # Stdin keeps the private key out of argv and plaintext files. Never expose
    # ssh-keygen's stderr: parse failures may include input-derived text.
    output, _error, status = Open3.capture3("ssh-keygen", "-y", "-P", "", "-f", "/dev/stdin", stdin_data: private_key)
    raise Invalid unless status.success?

    output.split.first(2).join(" ")
  rescue Errno::ENOENT
    raise Invalid
  end

  def self.fingerprint(public_key)
    raise Invalid if public_key.to_s.bytesize > 16.kilobytes

    parts = public_key.to_s.split
    raise Invalid unless parts.size >= 2 && %w[ssh-ed25519 ssh-rsa ecdsa-sha2-nistp256 ecdsa-sha2-nistp384 ecdsa-sha2-nistp521].include?(parts[0])

    wire = Base64.strict_decode64(parts[1])
    # Ask OpenSSH to validate the complete wire format, not just its base64.
    _output, _error, status = Open3.capture3("ssh-keygen", "-l", "-f", "/dev/stdin", stdin_data: parts.first(2).join(" "))
    raise Invalid unless status.success?

    "SHA256:#{Base64.strict_encode64(Digest::SHA256.digest(wire)).delete_suffix('=')}"
  rescue ArgumentError, Errno::ENOENT
    raise Invalid
  end
end
