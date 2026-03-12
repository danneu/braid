# btrfs RAID1 Deep Dive — Link Collection

A curated list of links for becoming a btrfs specialist, with emphasis on RAID1,
LUKS encryption, NixOS integration, and NAS operations.

Each link is prefixed with a unique slug for cross-referencing.

Each URL's html is saved locally to `./btrfs-links/{slug}.html`. For example, the slug `kerneldocs_btrfs`'s html will be in `./btrfs-links/kerneldocs_btrfs.html` for local reference.

---

## 1. Official Documentation

- [x] `kerneldocs_btrfs` [BTRFS — The Linux Kernel documentation](https://docs.kernel.org/filesystems/btrfs.html) — Primary official btrfs docs maintained as part of the Linux kernel; covers architecture, features, mount options.
- [x] `readthedocs_home` [Welcome to BTRFS documentation! (readthedocs)](https://btrfs.readthedocs.io/) — The new official documentation site, actively maintained, replacing the old kernel.org wiki.
- [x] `readthedocs_intro` [Introduction — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/Introduction.html) — Overview of btrfs as a COW filesystem, design goals, and key features.
- [x] `readthedocs_status` [Status — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/Status.html) — Feature stability matrix: RAID1 is stable; RAID5/6 are not recommended. Since kernel 6.12, CONFIG_BTRFS_EXPERIMENTAL gates in-development features.
- [x] `readthedocs_admin` [Administration — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/Administration.html) — Ongoing administration tasks: mount options, scrubbing, device management.
- [x] `readthedocs_volume_mgmt` [Volume management — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/Volume-management.html) — Multi-device management, adding/removing devices, RAID profile selection, profile conversion.
- [x] `readthedocs_balance` [Balance — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/Balance.html) — Balance operations for redistributing data across devices, converting between RAID profiles.
- [x] `readthedocs_scrub` [Scrub — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/Scrub.html) — How scrub works, what it detects, and how it auto-repairs from redundant copies in RAID profiles.
- [x] `readthedocs_compression` [Compression — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/Compression.html) — zstd levels, default level 3, guidance on level selection by workload.
- [x] `readthedocs_dedup` [Deduplication — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/Deduplication.html) — In-band vs. out-of-band deduplication concepts and available tools.
- [x] `readthedocs_subvolumes` [Subvolumes — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/Subvolumes.html) — Creation, deletion, nesting, relationship between subvolumes and snapshots.
- [x] `readthedocs_qgroups` [Quota groups — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/Qgroups.html) — Hierarchical quota enforcement, exclusive vs. shared space accounting, snapshot interaction.
- [x] `readthedocs_trim` [Trim/discard — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/Trim.html) — Synchronous discard, asynchronous discard (`discard=async`), and periodic fstrim.
- [x] `readthedocs_mount_options` [BTRFS mount options — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/ch-mount-options.html) — Canonical reference for all btrfs mount options.
- [x] `readthedocs_hardware` [Hardware considerations — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/Hardware.html) — Disk firmware bugs, write cache behavior, atomicity requirements, power protection for COW filesystems.
- [x] `readthedocs_troubleshooting` [Troubleshooting pages — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/trouble-index.html) — Common error conditions, diagnostic steps, and when to escalate to check/repair.
- [x] `readthedocs_glossary` [Glossary — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/Glossary.html) — Defines block groups, chunks, extents, COW, RAID profiles.
- [x] `readthedocs_design` [Btrfs design — BTRFS documentation](https://btrfs.readthedocs.io/en/latest/dev/dev-btrfs-design.html) — Developer-oriented docs on internal architecture and on-disk format.
- [x] `btrfs_wiki_home` [Btrfs Wiki — kernel.org (archived)](https://btrfs.wiki.kernel.org/) — The original btrfs wiki, archived but still accessible for historical reference.
- [x] `btrfs_wiki_faq` [FAQ — btrfs Wiki (archived)](https://archive.kernel.org/oldwiki/btrfs.wiki.kernel.org/index.php/FAQ.html) — Comprehensive archived FAQ covering behavior, limits, and common issues.
- [x] `btrfs_wiki_problem_faq` [Problem FAQ — btrfs Wiki (archived)](https://archive.kernel.org/oldwiki/btrfs.wiki.kernel.org/index.php/Problem_FAQ.html) — Troubleshooting common btrfs problems and error messages.
- [x] `btrfs_wiki_gotchas` [Gotchas — btrfs Wiki (archived)](https://archive.kernel.org/oldwiki/btrfs.wiki.kernel.org/index.php/Gotchas.html) — Duplicate UUIDs, free space reporting quirks, snapshot management pitfalls.
- [x] `btrfs_wiki_multi_device` [Using Btrfs with Multiple Devices — btrfs Wiki (archived)](https://archive.kernel.org/oldwiki/btrfs.wiki.kernel.org/index.php/Using_Btrfs_with_Multiple_Devices.html) — Multi-device usage, degraded mount semantics, the `missing` keyword.
- [x] `btrfs_wiki_sysadmin` [SysadminGuide — btrfs Wiki (archived)](https://archive.kernel.org/oldwiki/btrfs.wiki.kernel.org/index.php/SysadminGuide.html) — Sysadmin guide covering filesystem management, subvolumes, and administration.
- [x] `btrfs_wiki_production` [Production Users — btrfs Wiki](https://btrfs.wiki.kernel.org/index.php/Production_Users) — List of known production btrfs deployments.

## 2. Man Pages

### btrfs.readthedocs.io (official)

- [x] `readthedocs_man_index` [Manual pages index](https://btrfs.readthedocs.io/en/latest/man-index.html) — Master index of all btrfs man pages.
- [x] `readthedocs_man_btrfs8` [btrfs(8)](https://btrfs.readthedocs.io/en/latest/btrfs.html) — Top-level btrfs command, all subcommands and global options.
- [x] `readthedocs_man_btrfs5` [btrfs(5)](https://btrfs.readthedocs.io/en/latest/btrfs-man5.html) — Mount options, filesystem features, on-disk format details.
- [x] `readthedocs_man_mkfs` [mkfs.btrfs(8)](https://btrfs.readthedocs.io/en/latest/mkfs.btrfs.html) — Creating btrfs filesystems; all RAID profile options including raid1, raid1c3, raid1c4.
- [x] `readthedocs_man_balance` [btrfs-balance(8)](https://btrfs.readthedocs.io/en/latest/btrfs-balance.html) — Balance start/cancel/status/resume and filter options.
- [x] `readthedocs_man_device` [btrfs-device(8)](https://btrfs.readthedocs.io/en/latest/btrfs-device.html) — Device add, remove, delete, stats.
- [x] `readthedocs_man_filesystem` [btrfs-filesystem(8)](https://btrfs.readthedocs.io/en/latest/btrfs-filesystem.html) — df, du, show, usage, resize, defragment.
- [x] `readthedocs_man_scrub` [btrfs-scrub(8)](https://btrfs.readthedocs.io/en/latest/btrfs-scrub.html) — Checksum verification and automatic repair from RAID redundancy.
- [x] `readthedocs_man_replace` [btrfs-replace(8)](https://btrfs.readthedocs.io/en/latest/btrfs-replace.html) — Online device replacement; the `-r` flag to avoid reading from errored source.
- [x] `readthedocs_man_subvolume` [btrfs-subvolume(8)](https://btrfs.readthedocs.io/en/latest/btrfs-subvolume.html) — Subvolume create, delete, list, snapshot, show.
- [x] `readthedocs_man_check` [btrfs-check(8)](https://btrfs.readthedocs.io/en/latest/btrfs-check.html) — Offline filesystem checking and repair. Warning: "Do not use --repair unless advised by a developer."
- [x] `readthedocs_man_rescue` [btrfs-rescue(8)](https://btrfs.readthedocs.io/en/latest/btrfs-rescue.html) — Recovery tools: chunk-recover, super-recover, zero-log, fix-device-size, clear-uuid-tree, clear-space-cache.
- [x] `readthedocs_man_restore` [btrfs-restore(8)](https://btrfs.readthedocs.io/en/latest/btrfs-restore.html) — Non-destructive extraction of files from an unmountable btrfs filesystem.
- [x] `readthedocs_man_quota` [btrfs-quota(8)](https://btrfs.readthedocs.io/en/latest/btrfs-quota.html) — Quota enable/disable and the performance implications.
- [x] `readthedocs_man_qgroup` [btrfs-qgroup(8)](https://btrfs.readthedocs.io/en/latest/btrfs-qgroup.html) — Qgroup creation, assignment, limits, usage display.

### man7.org mirrors

- [x] `man7_btrfs8` [btrfs(8)](https://www.man7.org/linux/man-pages/man8/btrfs.8.html)
- [x] `man7_mkfs` [mkfs.btrfs(8)](https://man7.org/linux/man-pages/man8/mkfs.btrfs.8.html)
- [x] `man7_balance` [btrfs-balance(8)](https://man7.org/linux/man-pages/man8/btrfs-balance.8.html)
- [x] `man7_device` [btrfs-device(8)](https://man7.org/linux/man-pages/man8/btrfs-device.8.html)
- [x] `man7_filesystem` [btrfs-filesystem(8)](https://man7.org/linux/man-pages/man8/btrfs-filesystem.8.html)
- [x] `man7_scrub` [btrfs-scrub(8)](https://man7.org/linux/man-pages/man8/btrfs-scrub.8.html)
- [x] `man7_replace` [btrfs-replace(8)](https://man7.org/linux/man-pages/man8/btrfs-replace.8.html)
- [x] `man7_check` [btrfs-check(8)](https://man7.org/linux/man-pages/man8/btrfs-check.8.html)
- [x] `man7_rescue` [btrfs-rescue(8)](https://www.man7.org/linux/man-pages/man8/btrfs-rescue.8.html)
- [x] `man7_restore` [btrfs-restore(8)](https://man7.org/linux/man-pages/man8/btrfs-restore.8.html)
- [x] `man7_btrfstune` [btrfstune(8)](https://www.man7.org/linux/man-pages/man8/btrfstune.8.html)
- [x] `man7_fsck` [fsck.btrfs(8)](https://man7.org/linux/man-pages/man8/fsck.btrfs.8.html)

## 3. Changelogs and Version History

- [x] `readthedocs_changelog_kernel` [Changes (kernel/version)](https://btrfs.readthedocs.io/en/latest/Kernel-by-version.html) — Per-kernel-version changelog; the best reference for what changed in each Linux release.
- [x] `readthedocs_changelog_feature` [Changes (feature/version)](https://btrfs.readthedocs.io/en/latest/Feature-by-version.html) — Which btrfs features were introduced or stabilized in each kernel version.
- [x] `readthedocs_changelog_progs` [Changes (btrfs-progs)](https://btrfs.readthedocs.io/en/latest/CHANGES.html) — Userspace tools changelog.
- [x] `github_btrfs_progs_releases` [btrfs-progs Releases (GitHub)](https://github.com/kdave/btrfs-progs/releases) — Tagged releases with notes; major versions follow Linux kernel cycle (~2 months).
- [x] `btrfs_wiki_changelog` [Changelog — btrfs Wiki (archived)](https://archive.kernel.org/oldwiki/btrfs.wiki.kernel.org/index.php/Changelog.html) — Historical changelog from the original wiki.

## 4. Community Wikis and References

### Forza's Ramblings (wiki.tnonline.net) — excellent community reference

- [x] `forza_profiles` [Btrfs RAID profiles](https://wiki.tnonline.net/w/Btrfs/Profiles) — All allocation profiles, redundancy guarantees, and space efficiency.
- [x] `forza_features` [Btrfs features](https://wiki.tnonline.net/w/Btrfs/Features) — Overview of RAID, checksumming, snapshots, self-healing.
- [x] `forza_add_remove_replace` [Adding, removing and replacing devices](https://wiki.tnonline.net/w/Btrfs/Adding_and_removing_devices) — Practical device management guide.
- [x] `forza_replacing_disk` [Replacing a disk](https://wiki.tnonline.net/w/Btrfs/Replacing_a_disk) — Dedicated disk replacement guide; prefer `btrfs replace` over remove+add.
- [x] `forza_balance` [Balancing a Btrfs filesystem](https://wiki.tnonline.net/w/Btrfs/Balance) — When to balance, how it compacts chunks; warning not to balance metadata chunks regularly.
- [x] `forza_scrub` [Scrubbing a Btrfs filesystem](https://wiki.tnonline.net/w/Btrfs/Scrub) — Running scrub, scheduling, interpreting results.
- [x] `forza_enospc` [ENOSPC — No available disk space](https://wiki.tnonline.net/w/Btrfs/ENOSPC) — The most comprehensive ENOSPC guide: causes, diagnosis with `btrfs filesystem usage`, and recovery.
- [x] `forza_compression` [Btrfs compression](https://wiki.tnonline.net/w/Btrfs/Compression) — zstd, lzo, zlib with recommendations and gotchas.
- [x] `forza_mount_options` [Btrfs mount options](https://wiki.tnonline.net/w/Btrfs/Mount_Options) — Practical recommendations for each mount option.
- [x] `forza_send_receive` [Btrfs send and receive](https://wiki.tnonline.net/w/Btrfs/Send) — Send/receive commands, flags, and common patterns.
- [x] `forza_space_cache` [Space cache and free space tree](https://wiki.tnonline.net/w/Btrfs/Space_Cache) — space_cache v1 vs v2, why v2 is better for large filesystems, how to migrate.
- [x] `forza_checksums` [Btrfs checksum algorithms](https://wiki.tnonline.net/w/Btrfs/Checksum_Algorithms) — CRC32C, xxHash, SHA-256, BLAKE2b trade-offs.
- [x] `forza_dedup` [Btrfs deduplication](https://wiki.tnonline.net/w/Btrfs/Deduplication) — Comparing deduplication approaches and tools.
- [x] `forza_duperemove` [Duperemove](https://wiki.tnonline.net/w/Btrfs/Deduplication/Duperemove) — Duperemove-specific guide with performance tips.
- [x] `forza_getting_started` [Getting Started](https://wiki.tnonline.net/w/Btrfs/Getting_Started) — Introductory guide for beginners.
- [x] `forza_category_index` [Category index](https://wiki.tnonline.net/w/Category:Btrfs) — Full topic index.

### Distribution Wikis

- [x] `archwiki_btrfs` [Btrfs — ArchWiki](https://wiki.archlinux.org/title/Btrfs) — Comprehensive community-maintained reference.
- [x] `gentoowiki_btrfs` [Btrfs — Gentoo Wiki](https://wiki.gentoo.org/wiki/Btrfs) — Gentoo setup instructions with ENOSPC prevention and COW considerations.
- [x] `debianwiki_btrfs` [Btrfs — Debian Wiki](https://wiki.debian.org/Btrfs) — Debian-specific docs; RAID1 and RAID10 rated stable, RAID5/6 not recommended.
- [x] `wikipedia_btrfs` [Btrfs — Wikipedia](https://en.wikipedia.org/wiki/Btrfs) — History, development timeline, features, adoption status.
- [x] `archwiki_damaged_files` [Identify damaged files — ArchWiki](https://wiki.archlinux.org/title/Identify_damaged_files) — Using scrub output and kernel logs to find which files are affected by corruption.

### Meta (Facebook) at Scale

- [x] `meta_btrfs_facebook` [Btrfs at Facebook — Meta microsite](https://facebookmicrosites.github.io/btrfs/docs/btrfs-facebook.html) — Deployed on millions of servers for container root filesystems, build snapshots, zstd compression.
- [x] `meta_btrfs_landing` [Btrfs — Meta landing page](https://facebookmicrosites.github.io/btrfs/) — Meta's btrfs patches and usage documentation.
- [x] `meta_btrfs_docs` [Btrfs Documentation and Resources (Meta)](https://facebookmicrosites.github.io/btrfs/docs/btrfs-docs) — Meta's documentation reflecting their experience at scale.
- [x] `phoronix_meta_billions` [Btrfs Has Saved Meta "Billions Of Dollars" — Phoronix](https://www.phoronix.com/news/Btrfs-Saves-Meta-Billions) — Josef Bacik's statement that Meta's entire infrastructure runs on btrfs.
- [x] `lwn_btrfs_facebook` [Btrfs at Facebook — LWN.net](https://lwn.net/Articles/824855/) — LWN coverage of Meta's btrfs deployment.
- [x] `linuxcom_facebook_interview` [How Facebook Uses Linux and Btrfs — Linux.com](https://www.linux.com/news/how-facebook-uses-linux-and-btrfs-interview-chris-mason/) — Interview with original btrfs lead developer about Facebook's adoption.
- [x] `lf_video_facebook_scale` [Deploying Btrfs at Facebook Scale — Linux Foundation (video)](https://www.classcentral.com/course/youtube-deploying-btrfs-at-facebook-scale-josef-bacik-facebook-220818) — Conference talk by Josef Bacik.

## 5. RAID1 Setup Guides

- [x] `axllent_raid1_setup` [Setting up RAID1 with btrfs — Axllent.org](https://www.axllent.org/docs/btrfs-raid1/) — Concise walkthrough: two-disk RAID1, converting single to RAID1, fstab configuration.
- [x] `lexruee_arch_homeserver` [Arch Home Server with BTRFS RAID 1 — Lexruee's Blog](https://lexruee.ch/arch-home-server-with-btrfs-raid-1.html) — Full Arch home server walkthrough with maintenance.
- [x] `christitus_btrfs_guide` [BTRFS Guide — Chris Titus Tech](https://christitus.com/btrfs-guide/) — Practical guide covering commands, snapshots, and RAID setup.
- [x] `linuxhint_raid_setup` [How to Set Up Btrfs RAID — Linux Hint](https://linuxhint.com/set-up-btrfs-raid/) — Step-by-step tutorial for RAID0, RAID1, RAID10 with screenshots.
- [x] `internetvagabond_raid1` [Setting Up a BTRFS RAID-1 — The Internet Vagabond](https://www.theinternetvagabond.com/2020/06/14/setting-up-btrfs.html) — Personal RAID1 setup with practical tips.
- [x] `beginninglinux_raid1` [Setting up btrfs raid1 — BeginningLinux.com](https://www.beginninglinux.com/home/btrfs) — Beginner-friendly two-disk RAID1 guide.
- [x] `tuwien_raid1` [BTRFS and RAID1 — TU Wien](https://www.complang.tuwien.ac.at/anton/btrfs-raid1.html) — Technical notes on RAID1 behavior, performance, and gotchas.
- [x] `linuxnatives_perfect_setup` [The perfect Btrfs setup for a server — Linux Natives](https://linuxnatives.net/2016/perfect-btrfs-setup-for-a-server) — Server-focused setup with subvolume organization and maintenance.
- [x] `northerncoder_diy_nas` [Best Affordable DIY NAS with Btrfs Raid1 — northerncoder.ca](https://northerncoder.ca/best-affordable-diy-nas-with-btrfs-raid1-on-ubuntu-server/) — Complete DIY NAS build guide with hardware recommendations.
- [x] `gist_lucasmior_raid` [Btrfs and RAID — GitHub Gist (lucas-mior)](https://gist.github.com/lucas-mior/9aa3dc9e9185f083750c91821b447216) — Community reference summarizing btrfs RAID modes with examples.
- [x] `porcel_nas_setup` [How to setup a NAS using Btrfs — Nicolas Porcel](https://nicolas.porcel.me/posts/2016-12-10-setting-up-the-nas.html) — End-to-end NAS setup with RAID1 data and metadata profiles.

## 6. RAID1C3 / RAID1C4 (3-Copy and 4-Copy)

- [x] `phoronix_raid1c34` [Using Btrfs RAID1C3/RAID1C4 — Phoronix](https://www.phoronix.com/news/Using-Btrfs-RAID1C3-RAID1C4) — Practical guide to 3-copy and 4-copy profiles introduced in Linux 5.5.
- [x] `kdave_raid1c34` [Btrfs hilights in 5.5: raid1c3/c4 — kdave](https://kdave.github.io/btrfs-hilights-5.5-raid1c34/) — Developer blog post from the btrfs maintainer on feature design and rationale.
- [x] `lwn_55_merge_raid1c34` [5.5 Merge window, part 1 — LWN.net](https://lwn.net/Articles/806010/) — LWN coverage of the merge that introduced RAID1C3/C4.

## 7. LUKS + btrfs RAID1

- [x] `gist_maxxor_luks_raid` [LUKS-encrypted btrfs RAID volume — GitHub Gist (MaxXor)](https://gist.github.com/MaxXor/ba1665f47d56c24018a943bb114640d7) — Comprehensive gist covering setup, maintenance, and recovery. Directly relevant to braid.
- [x] `balaskas_raid1_luks` [BTRFS and RAID1 over LUKS — balaskas.gr](https://balaskas.gr/btrfs/raid1.html) — Mini-HOWTO with `-m raid1 -d raid1` command format.
- [x] `mutschler_ubuntu_luks_raid1` [Ubuntu btrfs-luks-RAID1 full disk encryption — mutschler.dev](https://mutschler.dev/linux/ubuntu-btrfs-raid1-20-04/) — Ubuntu guide with auto-snapshots.
- [x] `pentestpartners_luks2_raid1` [BTRFS RAID1 with LUKS2 FDE — Pen Test Partners](https://www.pentestpartners.com/security-blog/how-to-make-a-software-btrfs-raid1-with-luks2-fde/) — Security-focused guide.
- [x] `jorgensen_encrypted_array` [Encrypted Btrfs Array with LUKS — Kenneth Jorgensen](https://kennethjorgensen.com/blog/2022/encrypted-btrfs-array-with-luks/) — Clear explanations of the layering approach.
- [x] `yuuta_arch_raid_luks` [Arch Linux on Btrfs RAID with LUKS](https://blog.yuuta.moe/2021/12/25/arch-btrfs-raid-luks/) — Arch installation walkthrough.
- [x] `jlhinson_encrypted_monitored` [BTRFS RAID 1 array with encryption and monitoring — jlhinson.com](https://jlhinson.com/blog/2021-10-11-BTRFS-RAID-1-array-encrypted-monitored) — Includes monitoring and alerting.
- [x] `kneitinger_dmcrypt` [Btrfs on dm-crypt — Kyle Kneitinger](https://kneit.in/2015/09/17/brtfs-raid-on-dmcrypt.html) — Technical walkthrough of btrfs RAID on dm-crypt.
- [x] `starbeam_luks_btrfs` [Encrypting a disk with LUKS + Btrfs — Starbeamrainbowlabs](https://starbeamrainbowlabs.com/blog/article.php?article=posts/548-crypt-btrfs-setup.html) — Practical single-disk notes extensible to multi-device.
- [x] `gentoo_forum_luks_raid1` [btrfs raid1 with luks encryption — Gentoo Forums](https://forums.gentoo.org/viewtopic-t-1024794-start-0.html) — Distro-specific tips.
- [x] `manjaro_forum_convert_luks_raid1` [Converting single disk LUKS+btrfs to RAID1 — Manjaro Forum](https://forum.manjaro.org/t/btrfs-how-to-convert-single-disk-into-raid-1-with-luks/154272) — Conversion walkthrough.

## 8. Failure, Recovery, and Degraded Mode

- [x] `axllent_raid1_recovery` [Recover from a RAID1 failure — Axllent.org](https://www.axllent.org/docs/btrfs-raid1-recovery/) — Step-by-step: degraded mount, convert to single, remove failed device, re-establish RAID1. Critical pitfall: degraded mounts create single-profile block groups that must be rebalanced.
- [x] `linuxnatives_broken_disks` [Using RAID with btrfs and recovering from broken disks — Linux Natives](https://linuxnatives.net/2015/using-raid-btrfs-recovering-broken-disks) — Practical guide covering failure scenarios.
- [x] `programster_array_recovery` [Recover BTRFS Array After Device Failure — Programster's Blog](https://blog.programster.org/recover-btrfs-array-after-device-failure) — Concise degraded mount + device removal + replacement procedure.
- [x] `captnemo_dead_disks` [Dealing with dead disks in btrfs RAID1 — Nemo's blog](https://captnemo.in/blog/2019/02/24/btrfs-raid-device-replacement-story/) — Real-world recovery story with lessons learned.
- [x] `smlx_pool_recovery` [Recovering a btrfs pool after drive failure — smlx.dev](https://smlx.dev/posts/recovering-a-btrfs-pool-after-drive-failure/) — First-person account with commands and troubleshooting.
- [x] `btrfs_ml_replace_failed` [How to replace a failed drive in RAID1 — btrfs mailing list](https://www.spinics.net/lists/linux-btrfs/msg75681.html) — Developer-discussed correct procedure.
- [x] `opensuse_degraded_luks_raid1` [How do I boot into a degraded LUKS-encrypted Btrfs RAID 1? — openSUSE Forums](https://forums.opensuse.org/t/how-do-i-boot-into-a-degraded-luks-encrypted-btrfs-raid-1/175759) — Directly relevant to braid's architecture.
- [x] `opensuse_recover_encrypted_raid1` [How to recover encrypted btrfs RAID1 — openSUSE Forums](https://forums.opensuse.org/t/how-to-recover-encrypted-btrfs-raid1/173496) — LUKS-encrypted RAID1 recovery.
- [x] `proxmox_degraded_arrays` [How to deal with degraded btrfs-arrays? — Proxmox Forum](https://forum.proxmox.com/threads/how-to-deal-with-degraded-btrfs-arrays.91891/) — Includes systemd interactions that interfere with degraded mounts.
- [x] `proxmox_broken_disks` [Handle broken disks with BTRFS — Proxmox Forum](https://forum.proxmox.com/threads/handle-broken-disks-with-btrfs.126473/) — Multiple recovery approaches.
- [x] `archforum_repair_raid1` [How to repair broken btrfs of raid1 — Arch Forums](https://bbs.archlinux.org/viewtopic.php?id=198662) — Solved thread with commands and outcomes.
- [x] `btrfs_ml_degraded_mount_fail` [Trying to mount RAID1 degraded with removed disk — btrfs mailing list](https://linux-btrfs.vger.kernel.narkive.com/FXSNfyRM/trying-to-mount-raid1-degraded-with-removed-disk-open-ctree-failed) — Common failure mode discussion.
- `geekdiary_replace_failed` [How to replace a failed btrfs device — The Geek Diary](https://www.thegeekdiary.com/how-to-replace-a-failed-btrfs-device/) — Finding device IDs and post-replacement verification.
- [x] `ajfriesen_replacing_disks` [Replacing disks in BTRFS — ajfriesen.com](https://www.ajfriesen.com/replacing-disks-in-btrfs/) — Comparing `btrfs replace` vs. `device add` + `device delete`.
- [x] `btrfs_ml_remove_before_replace` [How to remove a device on RAID-1 before replacing — btrfs mailing list](https://linux-btrfs.vger.kernel.narkive.com/CsHmnSRa/how-to-remove-a-device-on-a-raid-1-before-replacing-it) — Proper sequence and pitfalls.
- [x] `btrfs_ml_remove_missing` [How to remove missing device on RAID1 — btrfs mailing list](https://www.spinics.net/lists/linux-btrfs/msg48434.html) — Minimum device count constraints.

## 9. Data Recovery Tools

- [x] `github_btrfscue` [btrfscue — GitHub](https://github.com/cblichmann/btrfscue) — Advanced recovery tool for restoring data from disk images of faulty devices.
- [x] `github_undelete_btrfs` [undelete-btrfs — GitHub](https://github.com/danthem/undelete-btrfs) — Automates recovery at multiple levels with btrfs restore.
- [x] `ufsexplorer_btrfs_raid` [How to recover data from Btrfs-RAID — UFS Explorer](https://www.ufsexplorer.com/articles/how-to/recover-data-btrfs-raid/) — Emphasis on imaging disks first.
- [x] `btrfs_wiki_restore` [Restore — btrfs Wiki (archived)](https://archive.kernel.org/oldwiki/btrfs.wiki.kernel.org/index.php/Restore.html) — Usage examples and tips for recovering specific files.
- [x] `manjaro_forum_rescue_data` [How to rescue data from a damaged btrfs volume — Manjaro Forum](https://forum.manjaro.org/t/how-to-rescue-data-from-a-damaged-btrfs-volume/79414) — Full recovery workflow: ddrescue, btrfs restore, rescue subcommands.
- [x] `commandmasters_rescue` [Navigating btrfs rescue operations — CommandMasters](https://commandmasters.com/commands/btrfs-rescue-linux/) — Practical examples for each rescue subcommand.
- [x] `suse_kb_recover_errors` [How to recover from BTRFS errors — SUSE KB](https://www.suse.com/support/kb/doc/?id=000018769) — Recommended order: scrub first, then check, repair as last resort.
- [x] `marc_scrub_repair` [Btrfs scrub and filesystem repair — Marc's Blog](https://marc.merlins.org/perso/btrfs/post_2014-03-19_Btrfs-Tips_-Btrfs-Scrub-and-Btrfs-Filesystem-Repair.html) — Regularly updated practical advice on scrub vs. check vs. repair.
- [x] `cubiclenate_corruption_fix` [Fixing btrfs corruption in single-user mode — CubicleNate](https://cubiclenate.com/2025/07/31/quick-fix-recover-a-corrupted-btrfs-filesystem-in-minutes/) — Practical steps with cautions.
- [x] `gist_bruvv_synology_repair` [Synology btrfs repair — GitHub Gist](https://gist.github.com/bruvv/d9edd4ad6d5548b724d44896abfd9f3f) — Community-maintained Synology repair steps.
- [x] `rockstor_data_loss` [Data loss prevention and recovery — Rockstor docs](https://rockstor.com/docs/data_loss.html) — Rockstor's official data loss guide.

## 10. Gotchas, Pitfalls, and Common Problems

### General Gotchas

- [x] `silvenga_lessons_learned` [BTRFS: Lessons Learned through Blood and Tears — Silvenga](https://silvenga.com/posts/btrfs-and-lessons-learned/) — Real-world failures including fragmentation, ENOSPC, and RAID issues.
- [x] `depau_troubleshooting` [Btrfs troubleshooting and tricks — Davide Depau](https://blog.depau.eu/2021/07/19/btrfs-troubleshooting/) — Practical troubleshooting guide.
- [x] `coldattic_do_not_use` [Do not use Btrfs! — coldattic.info](http://coldattic.info/post/70/) — Cautionary data loss account; catalog of failure modes.
- [x] `flatcar_troubleshooting` [Working with btrfs and common troubleshooting — Flatcar](https://www.flatcar.org/docs/latest/setup/debug/btrfs-troubleshooting/) — Focused troubleshooting for common operational issues.
- [x] `anarcat_btrfs_notes` [BTRFS notes — anarcat](https://anarc.at/blog/2022-05-13-brtfs-notes/) — Detailed personal notes on issues encountered.

### ENOSPC (No Space Left on Device)

- [x] `archforum_enospc_80pct` [BTRFS balance returns ENOSPC with 80% free — Arch Forums](https://bbs.archlinux.org/viewtopic.php?id=200926) — Real case study with solutions.
- [x] `fedora_discuss_enospc_profiles` [BTRFS "no space left on device" but there is enough — Fedora Discussion](https://discussion.fedoraproject.org/t/btrfs-problems-no-space-left-on-device-but-there-is-enough-multiple-block-group-profiles/74941) — Multiple block group profiles after degraded mount.
- [x] `suse_kb_enospc` [btrfs ENOSPC — SUSE KB](https://www.suse.com/support/kb/doc/?id=000018807) — Official diagnosis and resolution.
- [x] `lwn_metadata_enospc` [btrfs: proper metadata ENOSPC handling — LWN.net](https://lwn.net/Articles/348659/) — Kernel-level approach.
- [x] `infotinks_allocation_enospc` [btrfs allocation, freespace, and ENOSPC — infotinks](http://www.infotinks.com/btrfs-fi-df-balance-btrfs-df-and-freespace-req/) — `btrfs fi df` vs `df` and ENOSPC prevention.
- [x] `hugemanatee_emergency_space` [BTRFS and free space — emergency response](https://ohthehugemanatee.org/blog/2019/02/11/btrfs-out-of-space-emergency-response/) — Handling emergencies when snapshot accumulation prevents deletions.

### RAID1 Write Hole

- [x] `btrfs_ml_raid1_write_hole` [btrfs RAID1 is protected from write holes by COW — btrfs mailing list](https://www.spinics.net/lists/linux-btrfs/msg100951.html) — Both old and new states are valid until superblock commit, unlike RAID5/6.

### RAID5/6 Problems (Context for Why RAID1 Is Preferred)

- [x] `phoronix_raid56_warning` [Btrfs Will "Strongly Discourage" RAID5/RAID6 — Phoronix](https://www.phoronix.com/news/Btrfs-Warning-RAID5-RAID6) — Official warning added to mkfs.btrfs.
- [x] `linuxreviews_raid56` [Btrfs Was Not Meant For RAID5 or 6 — LinuxReviews](https://linuxreviews.org/Btrfs_Was_Not_Meant_For_RAID5_or_6) — Parity corruption bug where fixing one error can corrupt good blocks.
- [x] `marc_raid5_status` [btrfs RAID5 status — Marc's Blog](https://marc.merlins.org/perso/btrfs/post_2014-03-23_Btrfs-Raid5-Status.html) — Long-running tracking of slow progress and persistent issues.
- [x] `lwn_stripe_tree` [btrfs RAID stripe tree — LWN.net](https://lwn.net/Articles/944631/) — Key to eventually fixing the RAID5/6 write hole.
- [x] `lwn_stripe_tree_drafts` [RAID stripe tree draft patches — LWN.net](https://lwn.net/Articles/899392/) — Early draft patches for the fix.

### Mixed/Different Size Drives

- [x] `preining_mixed_sizes` [Multi-device and RAID1 with btrfs — preining.info](https://www.preining.info/blog/2020/05/multi-device-and-raid1-with-btrfs/) — Practical setup with varying sizes.
- [x] `archforum_nonuniform_drives` [Setting up raid1 or raid1c3 with 3-4 non-uniform drives — Arch Forums](https://bbs.archlinux.org/viewtopic.php?id=261626) — Configuration walkthrough.
- [x] `rockstor_different_sizes` [Multi device BTRFS with disks of different size — Rockstor Forum](https://forum.rockstor.com/t/multi-device-btrfs-filesystem-with-disk-of-different-size/5976) — Practical issues with mixed sizes.

### Quotas and Performance

- [x] `oracle_qgroup_vs_squota` [Btrfs Qgroup Quota vs. Simple Quota — Oracle](https://blogs.oracle.com/linux/btrfs-qgroup-quota-vs-simple-quota) — Why qgroups cause severe performance issues; how "simple quotas" (squotas) solve them.
- [x] `github_omarchy_quota_regression` [btrfs quota introduced heavy performance regression — GitHub](https://github.com/basecamp/omarchy/issues/3922) — Real-world severe performance degradation from quotas being enabled.
- [x] `sensille_qgroups_pdf` [Btrfs Subvolume Quota Groups (PDF) — Arne Jansen](http://sensille.com/qgroups.pdf) — Original design document for btrfs qgroups.

## 11. Balance Best Practices

- [x] `nvidia_when_to_rebalance` [When to Rebalance BTRFS Partitions — NVIDIA KB](https://docs.nvidia.com/networking-ethernet-software/knowledge-base/Configuration-and-Usage/Storage/When-to-Rebalance-BTRFS-Partitions/) — When rebalancing is needed vs. unnecessary.
- [x] `btrfs_wiki_balance_filters` [Balance Filters — btrfs Wiki](https://btrfs.wiki.kernel.org/index.php/Balance_Filters) — `-dusage`, `-musage` to target partially-filled chunks.
- [x] `techrepublic_rebalance` [How to rebalance your btrfs filesystem — TechRepublic](https://www.techrepublic.com/article/how-to-rebalance-your-btrfs-filesystem-on-your-linux-data-center-servers/) — Step-by-step for data center servers.
- [x] `warrenpost_snapshot_space` [Freeing space by deleting btrfs snapshots — warrenpost](https://warrenpost.wordpress.com/2016/04/28/freeing-space-by-deleting-btrfs-snapshots/) — Why free space may not appear after deletion; using balance to consolidate.

## 12. Subvolumes and Snapshots

### Subvolume Layout

- [x] `jwillikers_layout` [Btrfs Layout — JWillikers](https://www.jwillikers.com/btrfs-layout) — Flat vs. nested layouts; recommends flat with Ubuntu-style naming (@, @home).
- [x] `fedoramag_subvolumes` [Working with Btrfs Subvolumes — Fedora Magazine](https://fedoramagazine.org/working-with-btrfs-subvolumes/) — Beginner-friendly walkthrough.
- [x] `archforum_default_subvol` [Best practise btrfs default subvolume — Arch Forums](https://bbs.archlinux.org/viewtopic.php?id=267225) — Top-level (ID=5) vs. dedicated subvolume.
- [x] `xda_subvol_vs_partitions` [5 ways btrfs subvolumes differ from partitions — XDA](https://www.xda-developers.com/how-btrfs-subvolumes-differ-from-conventional-storage-partitions/) — Accessible comparison with traditional partitions.

### Snapshots

- [x] `ounapuu_snapshot_guide` [Oversimplified guide into btrfs snapshots](https://ounapuu.ee/posts/2022/04/05/btrfs-snapshots/) — Snapshot mechanics, CoW, space usage, limitations as backups.
- [x] `fedoramag_snapshots` [Working with Btrfs Snapshots — Fedora Magazine](https://fedoramagazine.org/working-with-btrfs-snapshots/) — Step-by-step tutorial for creating and using snapshots.
- [x] `lwn_subvol_snapshots` [Btrfs: Subvolumes and snapshots — LWN.net](https://lwn.net/Articles/579009/) — Internals: reference counting and CoW transactions.
- [x] `lwn_cow_snapshotting` [Btrfs: a copy on write, snapshotting FS — LWN.net](https://lwn.net/Articles/237904/) — Early article on COW architecture enabling instant snapshots.
- [x] `lwn_btrfs_intro` [The Btrfs filesystem: An introduction — LWN.net](https://lwn.net/Articles/576276/) — Design goals, CoW semantics, snapshot capabilities.
- [x] `oracle_advanced_btrfs` [How I Use Advanced Capabilities of Btrfs — Oracle](https://www.oracle.com/technical-resources/articles/it-infrastructure/admin-advanced-btrfs.html) — Enterprise-oriented snapshots, send/receive, RAID.

### Send/Receive (Incremental Backups)

- [x] `fedoramag_incremental_backup` [Incremental backups with Btrfs snapshots — Fedora Magazine](https://fedoramagazine.org/btrfs-snapshots-backup-incremental/) — Step-by-step `btrfs send -p` with parent snapshots.
- [x] `btrfs_wiki_incremental_backup` [Incremental Backup — btrfs Wiki (archived)](https://archive.kernel.org/oldwiki/btrfs.wiki.kernel.org/index.php/Incremental_Backup.html) — Shell script examples and caveats.
- [x] `marc_send_receive` [Fast incremental backups with btrfs send/receive — Marc's Blog](https://marc.merlins.org/perso/btrfs/post_2014-03-22_Btrfs-Tips_-Doing-Fast-Incremental-Backups-With-Btrfs-Send-and-Receive.html) — Practical tips, scripts, and lessons learned.
- [x] `starbeam_nas_backups` [NAS Backups with btrfs send/receive — Starbeamrainbowlabs](https://starbeamrainbowlabs.com/blog/article.php?article=posts/472-nas-backups-part-2-btrfs-send-recieve.html) — NAS-specific experience and scripts.
- [x] `oracle_remote_send_receive` [Btrfs send/receive for remote backup — Oracle](https://docs.oracle.com/en/learn/ol-btrfs-send/) — Secure remote backups over SSH.

## 13. Snapshot Backup Tools

### btrbk

- [x] `github_btrbk` [btrbk — GitHub](https://github.com/digint/btrbk) — The primary tool: snapshots + remote backups, retention policies, incremental send/receive. Written in Perl.
- [x] `btrbk_man_btrbk` [btrbk(1) man page](https://digint.ch/btrbk/doc/btrbk.1.html) — All subcommands and options.
- [x] `btrbk_man_conf` [btrbk.conf(5) man page](https://digint.ch/btrbk/doc/btrbk.conf.5.html) — Configuration: retention policies, SSH targets, snapshot groups.
- [x] `btrbk_faq` [btrbk FAQ](https://github.com/digint/btrbk/blob/master/doc/FAQ.md) — Common setup issues, SSH config, retention strategies.
- [x] `ounapuu_btrbk_awesome` [btrbk is awesome — techtipsy](https://ounapuu.ee/posts/2022/07/09/btrbk-is-awesome/) — Praises separation of snapshots from subvolumes and flexible retention.

### Snapper

- [x] `archwiki_snapper` [Snapper — ArchWiki](https://wiki.archlinux.org/title/Snapper) — Installation, configuration, timeline snapshots, pre/post snapshots, rollback.
- [x] `opensuse_snapper_tutorial` [openSUSE Snapper Tutorial](https://en.opensuse.org/openSUSE:Snapper_Tutorial) — Official tutorial.
- [x] `snapper_io_tutorial` [Snapper project tutorial](http://snapper.io/tutorial.html) — Configuration, snapshot types, comparison, rollback, cleanup.
- [x] `suse_snapper_concepts` [Basic Concepts of Snapper — SUSE](https://documentation.suse.com/smart/systems-management/html/snapper-basic-concepts/index.html) — Architecture, types, cleanup algorithms.
- [x] `jwillikers_snapper` [Btrfs Snapshot Management With Snapper — JWillikers](https://www.jwillikers.com/btrfs-snapshot-management-with-snapper) — Timeline settings, cleanup, systemd timer integration.

### Other Tools

- [x] `github_btrfs_backup` [btrfs-backup — GitHub](https://github.com/bob1de/btrfs-backup) — Python tool for incremental atomic backups.
- [x] `github_buttermanager` [buttermanager — GitHub](https://github.com/egara/buttermanager) — GUI for managing snapshots, balancing, and system upgrades.
- [x] `github_awesome_btrfs` [awesome-btrfs — GitHub](https://github.com/boredsquirrel/awesome-btrfs) — Curated list of btrfs tools, utilities, and resources.

## 14. Performance Tuning

### Mount Options

- [x] `jwillikers_mount_options` [Btrfs Mount Options — JWillikers](https://www.jwillikers.com/btrfs-mount-options) — Curated recommended options with discussion.
- [x] `dragas_best_practices` [BTRFS Best Practices — IT Notes](https://it-notes.dragas.net/2018/10/13/btrfs-best-pratices/) — Checklist-style best practices.
- [x] `archforum_nvme_best_practice` [Btrfs best practice M.2 NVMe SSD — Arch Forums](https://bbs.archlinux.org/viewtopic.php?id=294472) — Current best practices for NVMe.
- [x] `endeavouros_optimizations` [Considering btrfs optimizations — EndeavourOS Forum](https://forum.endeavouros.com/t/considering-some-btrfs-optimizations/27025) — Real-world optimization experiences.

### SSD / HDD Optimization

- [x] `redhat_ssd_optimization` [SSD Optimization — Red Hat 7](https://docs.redhat.com/en/documentation/red_hat_enterprise_linux/7/html/storage_administration_guide/btrfs-ssd-optimization) — `ssd`, `ssd_spread`, TRIM/discard.
- [x] `poespas_ssd_perf` [SSD Performance Optimization with btrfs — Poespas](https://blog.poespas.me/posts/2024/08/16/optimizing-ssd-performance-with-linux-btrfs/) — SSD-specific techniques.
- [x] `archforum_mixed_hdd_ssd` [btrfs RAID1 with slow HD and SSD — Arch Forums](https://bbs.archlinux.org/viewtopic.php?id=261693) — Maximizing performance in mixed HDD+SSD RAID1.

### Compression

- [x] `fedorawiki_zstd_analysis` [Btrfs zstd compression analysis — Fedora Wiki](https://fedoraproject.org/wiki/Changes/BtrfsByDefault/CompressionLevelAnalysis) — zstd:1 yields 40% savings vs uncompressed; zstd:9 only adds 3% more for much higher CPU.
- [x] `fedorawiki_transparent_compress` [BtrfsTransparentCompression — Fedora Wiki](https://fedoraproject.org/wiki/Changes/BtrfsTransparentCompression) — Rationale for choosing compress=zstd:1 as default.
- [x] `proxmox_compress_force_zstd` [compress-force=zstd:3 as standard? — Proxmox Forum](https://forum.proxmox.com/threads/compress-force-zstd-3-as-standard-for-btrfs.140731/) — compress vs compress-force tradeoffs.
- [x] `phoronix_615_zstd_realtime` [Btrfs Fast/Realtime Zstd Compression in Linux 6.15 — Phoronix](https://www.phoronix.com/news/Linux-6.15-Btrfs) — Fast/realtime zstd compression support.
- [x] `manjaro_forum_zstd_levels` [What level of zstd compression for BTRFS? — Manjaro Forum](https://forum.manjaro.org/t/what-level-of-compression-is-advantageous-for-btrfs/183098) — Benchmarks comparing levels.

### Space Cache

- [x] `forza_space_cache_v2_mkfs` [Space cache v2 with mkfs — Forza](https://wiki.tnonline.net/w/Blog/Btrfs_space_cache=v2_with_mkfs) — Creating filesystems with v2 from the start.
- [x] `phoronix_space_cache_bench` [Benchmarks of Space Cache Option — Phoronix](https://www.phoronix.com/vr.php?view=15570) — v1 vs v2 performance benchmarks.
- [x] `lwn_free_space_btree` [Free space B-tree — LWN.net](https://lwn.net/Articles/658778/) — Design and rationale behind space_cache v2.

## 15. LUKS Performance and Tuning

- [x] `hnyk_luks_btrfs_perf` [Performance cost of dm-crypt (LUKS) with btrfs on SSD — Daniel Hnyk](https://danielhnyk.cz/performance-cost-of-dm-crypt-luks-with-btrfs-on-sss/) — Minimal performance cost on SSDs with AES-NI.
- [x] `mayrhofer_ssd_bench` [SSD Linux benchmarking: filesystems and encryption — Rene Mayrhofer](https://www.mayrhofer.eu.org/post/ssd-linux-benchmark/) — Comprehensive benchmarks of filesystem + encryption combinations.
- [x] `scs_disk_encryption_impact` [Performance impact of disk encryption — Sovereign Cloud Stack](https://scs.community/2023/02/24/impact-of-disk-encryption/) — Sequential writes up to 79% loss, reads up to 53%; NVMe can saturate CPU under heavy load.
- [x] `cloudflare_disk_encryption` [Speeding up Linux disk encryption — Cloudflare Blog](https://blog.cloudflare.com/speeding-up-linux-disk-encryption/) — Kernel patches that can double encryption throughput.
- [x] `archwiki_dmcrypt` [dm-crypt/Device encryption — ArchWiki](https://wiki.archlinux.org/title/Dm-crypt/Device_encryption) — Cipher selection, key sizes, sector sizes, performance tuning.
- [x] `fedorawiki_luks_sector_size` [LUKS Encryption Sector Size — Fedora Wiki](https://fedoraproject.org/wiki/Changes/LUKSEncryptionSectorSize) — Why 4096-byte sector size in LUKS2 provides better performance.
- [x] `archwiki_advanced_format` [Advanced Format — ArchWiki](https://wiki.archlinux.org/title/Advanced_Format) — Proper alignment for 4K-sector drives, critical for LUKS + btrfs.

### TRIM with LUKS + btrfs

- [x] `archforum_trim_luks_btrfs` [SSD Trimming with BTRFS and LUKS — Arch Forums](https://bbs.archlinux.org/viewtopic.php?id=297449) — Enabling TRIM passthrough with `allow-discards`.
- [x] `jaytaala_periodic_trim_luks` [Enable periodic TRIM on LUKS — jaytaala.com](https://confluence.jaytaala.com/display/TKB/Enable+periodic+TRIM+-+including+on+a+LUKS+partition) — fstrim.timer + LUKS allow-discards.
- [x] `endeavouros_fstrim_encrypted` [fstrim on encrypted btrfs SSD — EndeavourOS Forum](https://forum.endeavouros.com/t/trim-luks-fstrim-on-encrypted-btrfs-ssd-disk-partition/16010) — Setup guide for periodic TRIM on encrypted SSDs.

## 16. Scrub and Maintenance

- [x] `github_btrfsmaintenance` [btrfsmaintenance — GitHub (kdave)](https://github.com/kdave/btrfsmaintenance) — Official maintenance scripts for periodic scrub, balance, trim, defrag with systemd timer integration.
- [x] `jwillikers_scrub` [Btrfs Scrub — JWillikers](https://www.jwillikers.com/btrfs-scrub) — Setting up scrub with systemd timers.
- [x] `wafatech_monitoring` [Monitoring btrfs for anomalies — WafaTech](https://wafatech.sa/blog/linux/linux-security/monitoring-btrfs-for-anomalies-best-practices-for-linux-servers/) — Scrub scheduling, log monitoring, anomaly detection.
- [x] `xeome_maintenance` [Btrfs Maintenance — xeome.dev](https://notes.xeome.dev/notes/Btrfs-Maintenance) — Concise reference including balance with usage filters.
- [x] `gist_ricco386_scrub_timer` [Systemd timer for BTRFS scrub — GitHub Gist](https://gist.github.com/ricco386/4dd6cae0ad4e0397a5ba7a1e4b5b1f6d) — Ready-to-use templated systemd service and timer units.
- [x] `archforum_scrub_timer` [btrfs-scrub@.timer — Arch Forums](https://bbs.archlinux.org/viewtopic.php?id=281342) — How Arch's built-in timer works.
- [x] `gist_bmarwell_scrub_email` [Btrfs scrub with systemd timers + email — GitHub Gist](https://gist.github.com/bmarwell/fdf799bdb61f70a2dce089dba94f838c) — Weekly scrub with email notifications.
- [x] `fedora_discuss_scrub_balance_timers` [How to enable scrub and balance timers — Fedora Discussion](https://discussion.fedoraproject.org/t/how-to-enable-scrub-and-balance-timers/77336) — Fedora-specific systemd timers.
- [x] `akoch_scrub_systemd` [BTRFS Scrubbing via Systemd Timers — Alexander Koch](https://blog.alexanderkoch.net/2015/06/btrfs-scrubbing-via-systemd-timers.html) — Detailed write-up on calendar events for periodic scrubs.

## 17. Monitoring Tools

- [x] `github_checkmk_btrfs` [check_mk-btrfs_health — GitHub](https://github.com/edvler/check_mk-btrfs_health) — CheckMK plugin for scrub status, device errors, block allocation.
- [x] `munin_btrfs_stats` [btrfs_device_stats Munin plugin](https://gallery.munin-monitoring.org/plugins/munin-contrib/btrfs_device_stats/) — Graphing device error statistics over time.
- [x] `datadog_btrfs` [Btrfs Datadog Integration](https://docs.datadoghq.com/integrations/btrfs/) — Built-in Datadog Agent integration for usage metrics.
- [x] `programster_cheatsheet` [BTRFS Cheatsheet — Programster](https://blog.programster.org/btrfs-cheatsheet) — Quick reference for common commands including health checks.
- [x] `github_btrfs_status` [btrfs-status — GitHub](https://github.com/mosteo/btrfs-status) — Independent feature stability assessment.

## 18. Deduplication

- [x] `github_duperemove` [duperemove — GitHub](https://github.com/markfasheh/duperemove) — Finds duplicated extents and submits for kernel-level deduplication.
- [x] `duperemove_docs` [duperemove docs](https://markfasheh.github.io/duperemove/duperemove.html) — Usage, hashfile-based incremental operation.
- [x] `gentoowiki_duperemove` [Duperemove — Gentoo Wiki](https://wiki.gentoo.org/wiki/Duperemove) — Installation and usage instructions.
- [x] `linuxhint_dedup` [Save disk space with btrfs deduplication — Linux Hint](https://linuxhint.com/save-disk-space-btrfs-deduplication/) — Practical deduplication tutorial.

## 19. NixOS Integration

### NixOS + btrfs

- [x] `nixoswiki_btrfs` [Btrfs — Official NixOS Wiki](https://wiki.nixos.org/wiki/Btrfs) — `boot.supportedFilesystems`, scrub service, mount options.
- [x] `nixoswiki_community_btrfs` [Btrfs — NixOS Wiki (community)](https://nixos.wiki/wiki/Btrfs) — Community config examples and tips.
- [x] `github_nixpkgs_btrfs_module` [nixpkgs btrfs.nix module source](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/tasks/filesystems/btrfs.nix) — Source implementing `services.btrfs.autoScrub`.
- [x] `github_nixpkgs_scrub_shutdown` [services.btrfs.autoScrub: scrub prevents shutdown (nixpkgs #79017)](https://github.com/NixOS/nixpkgs/issues/79017) — Known issue to be aware of.
- [x] `mtcaret_optin_state` [Encrypted Btrfs Root with Opt-in State on NixOS — mt-caret](https://mt-caret.github.io/blog/posts/2020-06-29-optin-state.html) — Influential post on btrfs + impermanence.
- [x] `c8h4_nixos_btrfs` [nixos btrfs install guide — c8h4.io](https://c8h4.io/nixos-btrfs/) — Step-by-step NixOS on btrfs with subvolumes.
- [x] `discourse_nixos_btrfs_advice` [Btrfs seeking installation advice — NixOS Discourse](https://discourse.nixos.org/t/btrfs-seeking-installation-advice/40826) — Best practices for subvolume layout.
- [x] `solene_continuous_snapshots` [Linux BTRFS continuous snapshots — Solene](https://dataswamp.org/~solene/2022-10-07-nixos-btrfs-continuous-snapshots.html) — Near-real-time local backup on NixOS.

### NixOS + LUKS + btrfs

- [x] `nixoswiki_fde` [Full Disk Encryption — NixOS Wiki](https://nixos.wiki/wiki/Full_Disk_Encryption) — Overview of all FDE approaches on NixOS.
- [x] `haseebmajid_disko_luks` [BTRFS and LUKS on NixOS Using Disko — Haseeb Majid](https://haseebmajid.dev/posts/2024-07-30-how-i-setup-btrfs-and-luks-on-nixos-using-disko/) — Declarative disk partitioning.
- [x] `hubrecht_nixos_luks_btrfs` [NixOS installation (LUKS and BTRFS) — hubrecht.ovh](https://hubrecht.ovh/posts/nixos-01/) — Step-by-step installation.
- [x] `gist_le0xff_nixos_luks2` [NixOS + LUKS2 + BTRFS + systemd-boot — GitHub Gist](https://gist.github.com/Le0xFF/21942ab1a865f19f074f13072377126b) — Complete FDE installation gist.
- `weiseguy_nixos_luks_btrfs` [NixOS with LUKS and BTRFS — weiseguy.net](https://weiseguy.net/posts/how-to/setup/nixos-with-luks-and-btrfs/) — Concise setup guide.
- [x] `gist_hadilq_luks_lvm_btrfs` [Encrypted LUKS LVM Btrfs Root with Opt-in State — GitHub Gist](https://gist.github.com/hadilq/a491ca53076f38201a8aa48a0c6afef5) — LUKS+LVM+btrfs+impermanence.
- [x] `discourse_nixos_luks_tpm2` [NixOS with Btrfs, LVM, LUKS, TPM2 — NixOS Discourse](https://discourse.nixos.org/t/install-nixos-with-btrfs-lvm-on-luks-using-tpm2-to-start-up-with-support-for-suspend-to-disk/59735) — Advanced setup with TPM2 auto-unlock.
- [x] `notashelf_impermanence` [Full Disk Encryption and Impermanence — NotAShelf](https://notashelf.dev/posts/impermanence) — FDE + btrfs + impermanence.
- [x] `laniakita_fde_tpm2` [NixOS FDE + Secure Boot + TPM2 — Lani Akita](https://laniakita.com/blog/nixos-fde-tpm-hm-guide) — End-to-end LUKS + btrfs + TPM2 + secure boot.
- [x] `nixoswiki_yubikey_fde` [Yubikey-based FDE on NixOS — Official Wiki](https://wiki.nixos.org/wiki/Luks-based_FDE_with_Yubikey_PBA_and_btrfs_on_UEFI_NixOS) — YubiKey challenge-response.
- `tiredofit_encrypted_impermanence` [Installing NixOS: Encrypted BTRFS Impermanence — tiredofit](https://notes.tiredofit.ca/books/linux/page/installing-nixos-encrypted-btrfs-impermanance) — Encrypted btrfs + impermanence.
- [x] `0xdade_framework_disko` [Framework and NixOS: Declarative Encrypted Disk Partitions — 0xda.de](https://0xda.de/blog/2024/06/framework-and-nixos-declarative-encrypted-disk-partitions/) — Disko with encrypted btrfs.
- [x] `github_disko_encrypted_raid1` [Encrypted btrfs raid1 — disko issue #799](https://github.com/nix-community/disko/issues/799) — Declarative encrypted RAID1 discussion.
- [x] `github_nixpkgs_luks_btrfs_bug` [NixOS fails to load LUKS encrypted btrfs — nixpkgs #303246](https://github.com/NixOS/nixpkgs/issues/303246) — Bug report with diagnostics and workarounds.

### NixOS disko

- [x] `github_disko` [disko — GitHub](https://github.com/nix-community/disko) — Declarative disk partitioning and formatting using Nix expressions.
- [x] `github_disko_example_luks_btrfs` [disko example: luks-btrfs-subvolumes.nix](https://github.com/nix-community/disko/blob/master/example/luks-btrfs-subvolumes.nix) — Reference: LUKS-encrypted btrfs with subvolumes.
- [x] `github_disko_example_btrfs` [disko example: btrfs-subvolumes.nix](https://github.com/nix-community/disko/blob/master/example/btrfs-subvolumes.nix) — Reference: plain btrfs subvolume layout.

### NixOS btrbk

- [x] `nixoswiki_btrbk` [Btrbk — Official NixOS Wiki](https://wiki.nixos.org/wiki/Btrbk) — NixOS-specific btrbk configuration.
- [x] `nixoswiki_community_btrbk` [Btrbk — NixOS Wiki (community)](https://nixos.wiki/wiki/Btrbk) — Configuration snippets for local and remote backup.
- [x] `github_nixpkgs_btrbk_module` [nixpkgs btrbk.nix module source](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/services/backup/btrbk.nix) — NixOS module source for btrbk.
- [x] `discourse_nixos_backup_tools` [Btrfs backup tools in NixOS — NixOS Discourse](https://discourse.nixos.org/t/btrfs-backup-tools-in-nixos/11364) — Comparing tools available in nixpkgs.

## 20. btrfs-progs (Userspace Tools)

- [x] `github_btrfs_progs` [btrfs-progs — GitHub (kdave)](https://github.com/kdave/btrfs-progs) — Official development repository.
- [x] `github_btrfs_progs_status` [btrfs-progs Status.rst](https://github.com/kdave/btrfs-progs/blob/devel/Documentation/Status.rst) — Very latest stability ratings.

## 21. Comparisons (btrfs vs ZFS vs mdadm)

- [x] `capi_btrfs_vs_mdadm` [btrfs raid1 vs. mdadm raid1 — Capi's Corner](https://www.dont-panic.cc/capi/2023/10/19/btrfs-raid1-vs-mdadm-raid1/) — Direct technical comparison: checksumming, degraded behavior, recovery.
- [x] `level1techs_zfs_btrfs_mdadm` [ZFS vs BTRFS vs mdadm? — Level1Techs Forums](https://forum.level1techs.com/t/zfs-vs-btrfs-vs-mdadm/237345) — In-depth community comparison.
- [x] `lowendtalk_raid1_comparison` [Practical experience of RAID1 btrfs, ZFS, mdadm — LowEndTalk](https://lowendtalk.com/discussion/182983/practical-experience-of-raid1-btrfs-zfs-and-mdadm-raid1) — Hands-on experience including failure and recovery.
- [x] `diskinternals_zfs_btrfs_raid` [ZFS vs Btrfs vs RAID — DiskInternals](https://www.diskinternals.com/raid-recovery/zfs-vs-btrfs-vs-raid/) — Structured comparison.
- [x] `purestorage_btrfs_vs_zfs` [Btrfs vs. ZFS — Pure Storage Blog](https://blog.purestorage.com/purely-educational/btrfs-vs-zfs/) — Enterprise vendor comparison.
- [x] `wundertech_btrfs_vs_zfs` [Btrfs vs. ZFS in 2025 — WunderTech](https://www.wundertech.net/btrfs-vs-zfs-comparison/) — Home/prosumer focus.
- [x] `xda_btrfs_over_zfs` [7 reasons I chose Btrfs over ZFS for my home NAS — XDA](https://www.xda-developers.com/why-i-chose-btrfs-over-zfs-for-home-nas/) — Flexibility and lower resource requirements.
- [x] `ligabue_btrfs_zfs_ext4` [btrfs vs zfs vs ext4 — Alessio Ligabue](https://www.alessioligabue.it/en/blog/btrfs-zfs-ext4-comparison) — Three-way comparison with benchmarks.
- [x] `archforum_btrfs_or_mdadm` [Raid1 — btrfs or mdadm? — Arch Forums](https://bbs.archlinux.org/viewtopic.php?id=285266) — Practical advice on choosing.
- [x] `level1techs_zfs_vs_btrfs_pool` [Torn Between ZFS and BTRFS — Level1Techs](https://forum.level1techs.com/t/torn-between-zfs-and-btrfs-for-a-new-general-purpose-storage-pool-need-advice/188789) — Pool management, snapshots, send/receive.
- [x] `usercomp_selfhealing_mdadm` [BTRFS Self-Healing with mdadm RAID — UserComp](https://usercomp.com/news/1416604/btrfs-self-healing-with-mdadm-raid) — Using btrfs checksumming on top of mdadm.

## 22. In-Depth Articles and Analysis

- [x] `arstechnica_examining_btrfs` [Examining btrfs, Linux's perpetually half-finished filesystem — Ars Technica](https://arstechnica.com/gadgets/2021/09/examining-btrfs-linuxs-perpetually-half-finished-filesystem/) — Jim Salter's comprehensive analysis. Widely referenced.
- [x] `osnews_examining_btrfs` [Examining btrfs — OSnews](https://www.osnews.com/story/133987/examining-btrfs-linuxs-perpetually-half-finished-filesystem/) — Mirror/discussion of the Ars Technica piece.
- [x] `lwn_btrfs_history` [A short history of btrfs — LWN.net](https://lwn.net/Articles/342892/) — History and design goals.
- [x] `lwn_raid1_balancing` [RAID1 balancing methods — LWN.net](https://lwn.net/Articles/998310/) — New RAID1 read balancing strategies (rotation, latency, devid).
- [x] `both_org_wont_use_btrfs` [Why I won't use BtrFS — Both.org](https://www.both.org/?p=8150) — Critical perspective on shortcomings.

## 23. Hacker News Discussions

- [x] `hn_examining_btrfs` [Examining btrfs (2021)](https://news.ycombinator.com/item?id=28641170) — Large thread on RAID stability, production use, ZFS comparisons.
- [x] `hn_raid1_two_copies` [btrfs RAID1 means 2 copies, not mirror all devices](https://news.ycombinator.com/item?id=28643820) — Clarifying RAID1 semantics + RAID1C3/C4.
- [x] `hn_raid1_limitations` [btrfs RAID1 limitations](https://news.ycombinator.com/item?id=28642627) — Only 2 copies regardless of device count, degraded mount constraints.
- [x] `hn_different_sized_disks` [Killer feature: RAID1 with different sized disks](https://news.ycombinator.com/item?id=41076829) — Mixed drive sizes as a major advantage.
- [x] `hn_less_reliable_than_zfs` [btrfs is less reliable than ZFS](https://news.ycombinator.com/item?id=41077071) — Strongly critical take with counterarguments.
- [x] `hn_facebook_uses_btrfs` [Facebook uses btrfs](https://news.ycombinator.com/item?id=41078682) — What Meta's deployment proves.
- [x] `hn_production_everywhere` [btrfs is in production all over the place (2024)](https://news.ycombinator.com/item?id=42290629) — Defending production readiness.
- [x] `hn_unreliable_fud` [I routinely hear btrfs is unreliable...](https://news.ycombinator.com/item?id=19932196) — Pushback on FUD; war stories both ways.
- [x] `hn_production_for_years` [Using btrfs in production for years](https://news.ycombinator.com/item?id=16799787) — Positive long-term report.
- [x] `hn_project_health` [Is the btrfs project healthy?](https://news.ycombinator.com/item?id=19931812) — Development pace and project health.
- [x] `hn_62_improvements` [btrfs in Linux 6.2 improvements](https://news.ycombinator.com/item?id=33961106) — RAID5/6 fixes and performance.
- [x] `hn_why_not_btrfs_zfs` [Why wouldn't you use btrfs or zfs? (2024)](https://news.ycombinator.com/item?id=38849735) — Filesystem debate with RAID1 discussion.
- [x] `hn_raid56_write_hole` [btrfs RAID5/6 write hole](https://news.ycombinator.com/item?id=22005823) — Why RAID5/6 is dangerous; Synology uses mdadm underneath.
- [x] `hn_bug_hunting` [Bug hunting in btrfs (2024)](https://news.ycombinator.com/item?id=39765715) — Bug hunting and code quality.
- [x] `hn_freebsd_btrfs` [btrfs read-write on FreeBSD](https://news.ycombinator.com/item?id=44468308) — Portability beyond Linux.

## 24. Forum Discussions (Reddit, Level1Techs, Proxmox, etc.)

- [x] `proxmox_btrfs_vs_zfs_root` [Btrfs vs ZFS on RAID1 root — Proxmox Forum](https://forum.proxmox.com/threads/btrfs-vs-zfs-on-raid1-root-partition.124281/) — Real-world tradeoffs.
- [x] `proxmox_btrfs_production` [BTRFS in production — Proxmox Forum](https://forum.proxmox.com/threads/btrfs-in-production.103666/) — Production experiences.
- [x] `proxmox_raid1_homeserver` [RAID1 for homeserver — Proxmox Forum](https://forum.proxmox.com/threads/raid1-for-homeserver-zfs-btrfs-or-ext4.157572/) — Home server comparison.
- [x] `fedora_discuss_selfhealing` [btrfs raid1 for self-healing — Fedora Discussion](https://discussion.fedoraproject.org/t/btrfs-raid1-for-fully-self-healing-usage/162343) — Auto-repair via checksums + scrub.
- [x] `fedora_discuss_surrender` [BTRFS — I surrender! — Fedora Discussion](https://discussion.fedoraproject.org/t/btrfs-i-surrender-good-features-but-complex-wastes-time-not-kiss-does-not-play-well/81773) — Complexity and operational pitfalls.
- [x] `level1techs_raid_confusion` [BTRFS RAID Confusion — Level1Techs](https://forum.level1techs.com/t/btrfs-raid-confusion/204194) — How RAID1 always stores exactly 2 copies regardless of disk count.
- [x] `level1techs_different_drives` [RAID1 with 2 different same-size drives — Level1Techs](https://forum.level1techs.com/t/is-there-a-downside-of-using-raid-1-on-btrfs-with-2-different-but-same-size-drives/213184) — Mismatched brand/model drives.
- [x] `level1techs_first_nas` [First home NAS — btrfs questions — Level1Techs](https://forum.level1techs.com/t/first-home-build-nas-btrfs-questions/192015) — New builder with experienced responses.
- [x] `unraid_btrfs_raid1` [BTRFS RAID1 Configuration? — Unraid Forums](https://forums.unraid.net/topic/124753-btrfs-raid1-configuration/) — Unraid ecosystem.
- [x] `omv_raid1_setup` [BTRFS Raid1 Setup — OpenMediaVault Forums](https://forum.openmediavault.org/index.php/Thread/7229-BTRFS-Raid1-Setup/) — OMV interface.
- [x] `gottlieb_production_2013` [btrfs production experiences — Derek Gottlieb (2013)](https://thoughts.derekgottlieb.com/blog/2013/04/29/btrfs-production-experiences/) — Early production experience.
- [x] `slashdot_feel_about_btrfs` [How Do You Feel About Btrfs? — Slashdot (2020)](https://linux.slashdot.org/story/20/10/24/0146258/slashdot-asks-how-do-you-feel-about-btrfs) — Wide-ranging community opinions.
- [x] `lemmy_btrfs_raid_guidance` [Seeking guidance on BTRFS RAID — Lemmy](https://mbin.launay.org/m/linux@lemmy.ml/t/5658/Seeking-guidance-on-BTRFS-RAID) — Practical profile selection advice.
- [x] `rockstor_raid1_recovery` [Not sure how to recover from Raid1 failure — Rockstor Forum](https://forum.rockstor.com/t/solved-not-sure-how-to-recover-from-raid1-drive-failure/9642) — Solved recovery thread.
- [x] `fedora_discuss_backup_tools` [Btrfs snapshot backup solutions — Fedora Discussion](https://discussion.fedoraproject.org/t/does-anyone-have-a-good-simple-btrfs-snapshot-backup-solution/75544) — Community tool recommendations.
- [x] `endeavouros_btrbk_vs_snapborg` [btrbk vs snapborg — EndeavourOS Forums](https://forum.endeavouros.com/t/btrfs-backup-btrbk-vs-snapborg-what-does-snapborg-exactly/48405) — Tool comparison.
- [x] `grub_ml_btrfs_raid1` [GRUB + btrfs RAID1 — GRUB mailing list](https://www.mail-archive.com/bug-grub@gnu.org/msg17462.html) — Boot issues with RAID1.

## 25. Synology-Specific

- [x] `durst_synology_torture` [I couldn't break Synology SHR+btrfs (yet) — May Durst](https://daltondur.st/syno_btrfs_1/) — Torture-testing Synology's btrfs implementation.
- [x] `synology_btrfs_official` [How Btrfs protects your data — Synology](https://www.synology.com/en-us/dsm/Btrfs) — Official page on data protection features.
- [x] `synology_kb_raid_impl` [Synology RAID implementation for btrfs — Synology KB](https://kb.synology.com/en-nz/DSM/tutorial/What_was_the_RAID_implementation_for_Btrfs_File_System_on_SynologyNAS) — Key: Synology uses mdadm RAID underneath btrfs, not btrfs's own RAID.
- [x] `synoforum_degraded_raid` [When your RAID degrades with BTRFS in Synology — SynoForum](https://www.synoforum.com/resources/when-your-raid-degrade-with-btrfs-in-syno-nases.81/) — Degraded RAID handling on Synology.

## 26. Recent Kernel Developments (2024-2026)

- [x] `phoronix_619_btrfs` [Btrfs In Linux 6.19 — Phoronix](https://www.phoronix.com/news/Linux-6.19-Btrfs) — BS > PS improvements, checksum offloading (+15% direct I/O), scrub suspend/resume, FSCRYPT prep.
- [x] `phoronix_615_btrfs` [Btrfs in Linux 6.15 — Phoronix](https://www.phoronix.com/news/Linux-6.15-Btrfs) — Fast/realtime zstd compression and performance optimizations.
- [x] `packt_btrfs_perf` [Btrfs performance improvements — Packt](https://www.packtpub.com/en-pl/learning/tech-news/btrfs-makes-multiple-performance-improvements-to-be-shipped-in-the-next-linux-kernel-release) — Recent kernel cycle improvements.
- [x] `kerneldocs_btrfs_intree` [BTRFS — The Linux Kernel documentation](https://docs.kernel.org/filesystems/btrfs.html) — In-tree kernel docs covering sysfs interface and design notes.
