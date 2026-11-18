---
name: pi-health
description: Check a Raspberry Pi's health (temperature, throttling, power, memory, disk, load) and explain what's wrong in plain words.
---

Use when the user asks how the Pi is doing, why it's slow or hot, or for a health check.

## Steps
Run each as its own command (all are allowlisted, so no approval is needed):
1. `vcgencmd measure_temp`
2. `vcgencmd get_throttled`
3. `uptime` (load average: compare with the core count; a Pi 3/4 has 4 cores)
4. `free -m`
5. `df -h`
6. `ps -eo pid,user,comm,%cpu,%mem --sort=-%cpu` to find what's using the CPU

## Reading `get_throttled` (hex bits)
- 0x0 = all good.
- Bits 0–3 are happening **now**: 0x1 under-voltage, 0x2 ARM frequency capped, 0x4 throttled, 0x8 soft temperature limit.
- Bits 16–19 **happened since boot**: 0x10000 under-voltage, 0x20000 frequency capped, 0x40000 throttled, 0x80000 soft temperature limit.
- Any under-voltage bit means a weak power supply or cable. That causes crashes and SD-card corruption, so say this clearly.

## Temperatures
- Below 60 °C is fine, and 60–75 °C is warm but normal under load.
- At 80 °C the Pi throttles, and around 85 °C it throttles hard. A Pi 3B+ also lowers its clock at a soft limit of 60 °C.

## Answer
Give a short verdict first ("healthy", "running hot", or "power problem"), then the one or two causes that matter, with numbers. If a single process uses most of the CPU, name it. Suggest fixes; don't apply any without asking.
