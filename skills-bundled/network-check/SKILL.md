---
name: network-check
description: Diagnose "is the internet down?" step by step — link, local network, DNS, internet — and say which layer is broken.
---

Use when the user reports connection problems or asks whether the network is working.

## Steps
Work from the bottom up, and stop at the first layer that fails. These commands need approval, so run each one as a single command:
1. Link and address: `ip -brief addr` — does the interface have an IP? A `169.254.x.x` address means DHCP failed.
2. Gateway: `ip route` shows the gateway; then `ping -c 3 <gateway>`.
3. Internet by IP: `ping -c 3 1.1.1.1`
4. DNS: `getent hosts example.com`. If step 3 worked but this fails, it's a DNS problem.
5. Wi-Fi only: `iwconfig` or `nmcli device wifi` for signal strength, if installed.

## Reading results
| Symptom | Layer |
|---|---|
| No IP, or `169.254.x.x` | DHCP or router |
| Gateway doesn't answer | local network, Wi-Fi or cable |
| Gateway answers but 1.1.1.1 doesn't | the router's internet connection or the ISP |
| IPs work but names don't | DNS |
| Everything works | the problem is the specific service or site |

## Answer
Name the broken layer and the evidence (e.g. "DNS: 1.1.1.1 answers in 12 ms, but example.com doesn't resolve"), then the most likely fix.
