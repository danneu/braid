---
name: Dan's NAS build
description: Hardware specs for the 4x12TB NixOS NAS Dan is building with braid
type: project
---

Dan is building a 4x12TB NixOS NAS using braid. Total build cost ~$1690 ($690 hardware + $1000 drives).

**Hardware (ordered 2026-03-26):**
- **Mobo:** ASRock Industrial IMB-X1231 (W680, Mini-ITX, LGA 1700, 4x SATA, 2x M.2 Key M, 1x PCIe 4.0 x16, 2x DDR4 SO-DIMM ECC/non-ECC, 2x Intel 2.5GbE I225-LM/I226-LM)
- **CPU:** Intel i3-14100 (non-T, 60W base / 89W turbo — board caps at 65W, fine for NAS)
- **RAM:** A-Tech 8GB DDR4-3200 ECC SO-DIMM (1x8GB, 1Rx8) — one slot free for future upgrade
- **PSU:** Corsair SF750 (SFX)
- **NIC:** 10Gtek 10GbE PCIe NIC, single copper RJ45, Intel X540-BT1, PCIe x8 (in the x16 slot). May be optional since onboard has 2x 2.5GbE.
- **Drives:** 4x 12TB Toshiba N300 NAS Pro ($250 each from Walmart)

**Key decisions:**
- Chose IMB-X1231 over H610M-ITX/eDP for ECC support (W680 chipset)
- i3-14100 has UHD 730 iGPU for Intel Quick Sync hardware transcoding (Jellyfin/Plex)
- ECC SO-DIMMs are pricier than regular DIMMs but protect against in-flight RAM bit flips
- Onboard LAN1 (I225-LM/I226-LM) supports Wake-on-LAN and vPro
- Linux driver for onboard NICs: `igc`

**Why:** WoL is important (wake NAS remotely, then `braid unlock` over SSH).

**How to apply:** Reference this when making NixOS config decisions, driver choices, or hardware-specific braid features.
