# Low-end roles

These notes are operator guidance for the 0.3.0 soak. Product names here are acceptance context. They are not routing keys, and they are not a phone RAM table.

## Moto Z2

The Moto Z2 is Android 8. Termux bootstrap on that release is often painful. Use it as an RPC worker or a TUI client with a prebuilt `nexus` binary copied onto the device. Do not make it the orchestrator and do not put `prefill_device` on it.

## About 4 GB nodes

The Galaxy A17 and the 2009 Mac class are ~4 GB nodes. They run the local front door when a small GGUF fits, and they escalate when it does not. Doctor says `appliance: no local model fits` in that case. Tier-1 keywords and escalation still run.

## Worker RAM cap

Worker RAM uses the existing `cluster.max_rpc_ram_mb` default of 1800 MB so a 4 GB desktop stays responsive. That default is a starting cap for a small node. It is not a phone RAM table. Do not raise it on the 4 GB machine during the 0.3.0 soak.

## Penryn x86

Penryn x86 builds keep the rustflags in `.cargo/config.toml`. Do not install an AVX prebuilt `bmoe-cli` or `llama-server` on that machine.
