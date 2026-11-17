# Run from any directory: ruby contrib/chart/tests/linearbot_extra_ca_test.rb
require "minitest/autorun"
require "open3"
require "tempfile"
require "yaml"
require "digest"

class LinearbotExtraCaTest < Minitest::Test
  CHART = File.expand_path("..", __dir__)
  PEM = "-----BEGIN CERTIFICATE-----\nsynthetic-public-ca\n-----END CERTIFICATE-----\n".freeze

  def test_ca_bundle_is_mounted_and_trusted
    docs = render("enabled" => true, "extraCaCertificates" => PEM)
    pod = docs.find { |doc| doc["kind"] == "Deployment" }.fetch("spec").fetch("template")
    container = pod.fetch("spec").fetch("containers").first
    ca_map = docs.find { |doc| doc["kind"] == "ConfigMap" }
    assert_equal PEM, ca_map.fetch("data").fetch("extra-ca-certificates.pem")
    volume = pod.fetch("spec").fetch("volumes").find { |entry| entry["name"] == "extra-ca-certificates" }
    assert_equal ca_map.fetch("metadata").fetch("name"), volume.fetch("configMap").fetch("name")
    mount = container.fetch("volumeMounts").find { |entry| entry["name"] == "extra-ca-certificates" }
    assert_equal "/etc/centaur/ca", mount.fetch("mountPath")
    assert_equal true, mount.fetch("readOnly")
    assert_equal [ { "name" => "NODE_EXTRA_CA_CERTS", "value" => "/etc/centaur/ca/extra-ca-certificates.pem" } ],
      container.fetch("env").select { |entry| entry["name"] == "NODE_EXTRA_CA_CERTS" }
    assert_equal Digest::SHA256.hexdigest(PEM), pod.fetch("metadata").fetch("annotations").fetch("checksum/extra-ca-certificates")
  end

  def test_no_ca_resources_without_bundle
    docs = render("enabled" => true, "extraCaCertificates" => "")
    pod = docs.find { |doc| doc["kind"] == "Deployment" }.fetch("spec").fetch("template")
    container = pod.fetch("spec").fetch("containers").first
    assert_nil docs.find { |doc| doc["kind"] == "ConfigMap" }
    assert_nil container["volumeMounts"]
    assert_nil pod.fetch("spec")["volumes"]
    refute container.fetch("env").any? { |entry| entry["name"] == "NODE_EXTRA_CA_CERTS" }
    refute pod.fetch("metadata").fetch("annotations").key?("checksum/extra-ca-certificates")
  end

  def test_disabled_linearbot_emits_no_ca_configmap
    assert_empty render("enabled" => false, "extraCaCertificates" => PEM)
  end

  private

  def render(values)
    Tempfile.create(["linearbot-ca", ".yaml"]) do |file|
      file.write(YAML.dump("linearbot" => values))
      file.flush
      output, error, status = Open3.capture3("helm", "template", "ca-test", CHART, "-f", file.path)
      assert status.success?, error
      YAML.load_stream(output).compact.select { |doc| doc.dig("metadata", "name").to_s.include?("linearbot") }
    end
  end
end
