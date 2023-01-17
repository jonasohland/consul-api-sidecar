job "consul-api-agent" {
  datacenters = ["dc1"]
  type        = "system"
  group "agent" {
    network {
      mode = "host"
    }
    volume "host-services" {
      type      = "host"
      source    = "host-services"
      read_only = false
    }
    task "agent" {

      driver = "docker"

      config {
        network_mode = "host"
        image        = "jonasohland/consul-api-agent:0.0.1-distroless"
        args = [
          "--config", "${NOMAD_TASK_DIR}/config.toml",
          "--log-level", "debug"
        ]
      }

      volume_mount {
        volume      = "host-services"
        destination = "${NOMAD_TASK_DIR}/host-services"
      }

      template {
        change_mode   = "signal"
        change_signal = "SIGHUP"
        destination   = "${NOMAD_TASK_DIR}/config.toml"
        data          = <<EOF
[service.dns_1]
type    = "dns"
path    = "{{ env "NOMAD_TASK_DIR" }}/host-services/dns_1.sock"
address = "udp://127.0.0.53:53"
timeout = 150
EOF
      }

      resources {
        cpu    = 10
        memory = 50
      }
    }
  }
}
