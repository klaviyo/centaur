require "open3"
require "base64"
require "digest"

class SshKey
  class Invalid < StandardError; end

  def self.public_key(private_key)
    raise Invalid, "Paste your private key, including its BEGIN and END lines." if private_key.blank?
    raise Invalid, "The key is too large. Paste a single private key." if private_key.bytesize > 64.kilobytes

    key = normalize(private_key)
    if key.start_with?("ssh-", "ecdsa-", "-----BEGIN PUBLIC KEY-----")
      raise Invalid, "This is a public key. Paste the private key file, without the .pub extension."
    end
    unless key.match?(/\A-----BEGIN (?:OPENSSH |RSA |EC |DSA |ENCRYPTED )?PRIVATE KEY-----\n/)
      raise Invalid, "Include the complete private key, starting with its BEGIN PRIVATE KEY or BEGIN OPENSSH PRIVATE KEY line."
    end
    unless key.match?(/-----END (?:OPENSSH |RSA |EC |DSA |ENCRYPTED )?PRIVATE KEY-----\n\z/)
      raise Invalid, "The key is incomplete. Include its END PRIVATE KEY or END OPENSSH PRIVATE KEY line."
    end
    # Stdin keeps the private key out of argv and plaintext files. Never expose
    # ssh-keygen's stderr: parse failures may include input-derived text.
    output, error, status = Open3.capture3("ssh-keygen", "-y", "-P", "", "-f", "/dev/stdin", stdin_data: key)
    unless status.success?
      if encrypted?(key) || error.downcase.include?("passphrase")
        raise Invalid, "This key is protected by a passphrase. Use a dedicated key without a passphrase."
      end
      raise Invalid, "OpenSSH could not read this key. Copy the entire private key file, including both header lines and all contents."
    end

    output.split.first(2).join(" ")
  rescue Errno::ENOENT
    raise Invalid, "Key validation is temporarily unavailable. Try again later."
  end

  def self.normalize(private_key)
    private_key.to_s.gsub("\r\n", "\n").gsub("\r", "\n").strip + "\n"
  end

  def self.encrypted?(key)
    return true if key.include?("BEGIN ENCRYPTED PRIVATE KEY") || key.include?("Proc-Type: 4,ENCRYPTED")
    return false unless key.start_with?("-----BEGIN OPENSSH PRIVATE KEY-----")

    wire = Base64.strict_decode64(key.lines[1...-1].join.delete("\n"))
    magic = "openssh-key-v1\0"
    return false unless wire.start_with?(magic) && wire.bytesize >= magic.bytesize + 4

    length = wire.byteslice(magic.bytesize, 4).unpack1("N")
    cipher = wire.byteslice(magic.bytesize + 4, length)
    cipher.present? && cipher.bytesize == length && cipher != "none"
  rescue ArgumentError
    false
  end
  private_class_method :encrypted?

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
