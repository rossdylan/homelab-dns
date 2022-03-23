# homelab-dns
[![CircleCI](https://circleci.com/gh/rossdylan/homelab-dns/tree/main.svg?style=shield)](https://circleci.com/gh/rossdylan/homelab-dns/tree/main)

Simple authoritative DNS server for mapping a custom "external"
domain to a kubernetes LoadBalancer. Designed to be used with metallb.


## Notes On Homelab Setup
homelab-dns is designed explicitly with metallb and ubiquiti devices in mind.
The code itself is fairly agnostic, relying mostly on k8s built in objects.
The problem comes with the provided deployment manifests which are built explicitly for metallb.

## Edgerouter Configuration

1. Open Config Tree
2. Navigate to `service > dns > forwarding`
3. Setup DNS forwarding (if not already)
4. Under options click Add
5. Enter `server=/<your-selected-origin>/<external-ip-for-homelab-dns>`

## Usage
```
$ homelab-dns -h
homelab-dns 0.1
Ross Delinger <rossdylan@fastmail.com>
External-ish DNS for homelab kubernetes clusters

USAGE:
    homelab-dns [OPTIONS] --origin <ORIGIN>

OPTIONS:
    -b, --bind <BIND>                  Address and port to bind our DNS server on [default:
                                       127.0.0.1:53]
    -h, --help                         Print help information
    -o, --origin <ORIGIN>              The root domain we are serving DNS for
    -t, --ttl <TTL>                    Set the TTL of the LoadBalancer DNS records [default: 300]
        --tcp-timeout <TCP_TIMEOUT>    Timeout for DNS over TCP connections [default: 1s]
    -V, --version                      Print version information
```