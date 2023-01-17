job "consul-api-sidecar-example" {
  datacenters = ["dc1"]
  type        = "service"
  group "example" {

    network {
      mode = "bridge"
      dns {
        servers = ["127.0.0.1"]
      }
    }

    volume "host-services" {
      type      = "host"
      source    = "host-services"
      read_only = true
    }

    task "ubuntu" {
      driver = "docker"
      config {
        image = "ubuntu:focal"
        args  = ["/usr/bin/sleep", "Infinity"]
      }
    }

    task "consul-api-sidecar" {

      driver = "docker"

      config {
        image = "jonasohland/consul-api-sidecar:0.0.1-distroless"
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
listen  = "127.0.0.1:53"
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
