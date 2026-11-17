# Run from any directory: ruby contrib/chart/tests/slackbotv2_extra_ca_test.rb
require "minitest/autorun"
require "open3"
require "tempfile"
require "yaml"
require "digest"

class SlackbotExtraCaTest < Minitest::Test
  CHART = File.expand_path("..", __dir__)
  PEM = "-----BEGIN CERTIFICATE-----\nsynthetic-public-ca\n-----END CERTIFICATE-----\n".freeze

  [false, true].product([false, true]).each do |ca, proactive|
    define_method("test_ca_#{ca}_with_proactivity_#{proactive}") do
      docs = render("extraCaCertificates" => ca ? PEM : "", "proactivity" => {
        "enabled" => proactive, "defaultRules" => proactive ? [{ "name" => "test", "when" => "A test condition" }] : []
      })
      pod = docs.find { |doc| doc["kind"] == "Deployment" }.fetch("spec").fetch("template")
      container = pod.fetch("spec").fetch("containers").first
      mounts = container.fetch("volumeMounts", [])
      volumes = pod.fetch("spec").fetch("volumes", [])
      env = container.fetch("env")
      ca_map = docs.find { |doc| doc["kind"] == "ConfigMap" && doc.fetch("data").key?("extra-ca-certificates.pem") }
      assert_equal proactive, mounts.any? { |mount| mount["name"] == "proactivity-rules" }
      assert_equal proactive, volumes.any? { |volume| volume["name"] == "proactivity-rules" }
      assert_equal mounts.map { |mount| mount["name"] }.sort, volumes.map { |volume| volume["name"] }.sort
      if ca
        assert_equal PEM, ca_map.fetch("data").fetch("extra-ca-certificates.pem")
        volume = volumes.find { |entry| entry["name"] == "extra-ca-certificates" }
        assert_equal ca_map.fetch("metadata").fetch("name"), volume.fetch("configMap").fetch("name")
        mount = mounts.find { |entry| entry["name"] == "extra-ca-certificates" }
        assert_equal "/etc/centaur/ca", mount.fetch("mountPath")
        assert_equal true, mount.fetch("readOnly")
        assert_equal [ { "name" => "NODE_EXTRA_CA_CERTS", "value" => "/etc/centaur/ca/extra-ca-certificates.pem" } ],
          env.select { |entry| entry["name"] == "NODE_EXTRA_CA_CERTS" }
        assert_equal Digest::SHA256.hexdigest(PEM), pod.fetch("metadata").fetch("annotations").fetch("checksum/extra-ca-certificates")
      else
        assert_nil ca_map
        refute env.any? { |entry| entry["name"] == "NODE_EXTRA_CA_CERTS" }
        refute mounts.any? { |entry| entry["name"] == "extra-ca-certificates" }
        refute pod.fetch("metadata").fetch("annotations").key?("checksum/extra-ca-certificates")
      end
    end
  end

  def test_disabled_slackbot_emits_no_ca_configmap
    assert_empty render("enabled" => false, "extraCaCertificates" => PEM)
  end

  private

  def render(values)
    Tempfile.create(["slackbot-ca", ".yaml"]) do |file|
      file.write(YAML.dump("slackbotv2" => values))
      file.flush
      output, error, status = Open3.capture3("helm", "template", "ca-test", CHART, "-f", file.path)
      assert status.success?, error
      YAML.load_stream(output).compact.select { |doc| doc.dig("metadata", "name").to_s.include?("slackbotv2") }
    end
  end
end
