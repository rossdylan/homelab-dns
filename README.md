# homelab-dns
Dead simple authoritative DNS server for mapping a custom "external"
domain to a kubernetes LoadBalancer. Designed to be used with metallb
and ubiquiti edgerouters

```
rossdylan@cyrene ~/src/homelab-dns main! ○ ./target/debug/homelab-dns --bind 127.0.0.1:5053 --origin k8s.internal
2022-03-20T04:16:45.902048Z  INFO homelab_dns: added record for service grafana: grafana.k8s.internal -> 10.2.1.1
2022-03-20T04:17:02.237860Z  INFO trust_dns_server::server::server_future: request:29789 src:UDP://127.0.0.1#38027 QUERY:grafana.k8s.internal.:A:IN qflags:RD,AD response:NoError rr:1/0/0 rflags:RD,AA
```

```
rossdylan@cyrene ~/src/homelab-dns master! ○ dig @127.0.0.1 -p 5053 grafana.k8s.internal +short
10.2.1.1
```