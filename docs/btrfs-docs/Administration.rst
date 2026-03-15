Administration
==============

The main administration tool for BTRFS filesystems is :doc:`btrfs`.
Please refer to the manual pages of the subcommands for further documentation.
Other topics explaining features or concepts can be found in :doc:`btrfs-man5`.

Mount options
-------------

BTRFS SPECIFIC MOUNT OPTIONS
^^^^^^^^^^^^^^^^^^^^^^^^^^^^

This section describes mount options specific to BTRFS.  For the generic mount
options please refer to :manref:`mount(8)` manual page and also see the section
with BTRFS specifics :docref:`below<btrfs-man5:mount-options-generic>`. The options are
sorted alphabetically (discarding the *no* prefix).

.. note::
        Most mount options apply to the whole filesystem and only options in the
        first mounted subvolume will take effect. This is due to lack of implementation
        and may change in the future. This means that (for example) you can't set
        per-subvolume *nodatacow*, *nodatasum*, or *compress* using mount options. This
        should eventually be fixed, but it has proved to be difficult to implement
        correctly within the Linux VFS framework.

Mount options are processed in order, only the last occurrence of an option
takes effect and may disable other options due to constraints (see e.g.
*nodatacow* and *compress*). The output of :command:`mount` command shows which options
have been applied.

acl, noacl
        (default: on)

        Enable/disable support for POSIX Access Control Lists (ACLs).  See the
        :manref:`acl(5)` manual page for more information about ACLs.

        The support for ACL is build-time configurable (BTRFS_FS_POSIX_ACL) and
        mount fails if *acl* is requested but the feature is not compiled in.

.. duplabel:: mount-option-autodefrag

autodefrag, noautodefrag
        (since: 3.0, default: off)

        Enable automatic file defragmentation.
        When enabled, small random writes into files (in a range of tens of kilobytes,
        currently it's 64KiB) are detected and queued up for the defragmentation process.
        May not be well suited for large database workloads.

        The read latency may increase due to reading the adjacent blocks that make up the
        range for defragmentation, successive write will merge the blocks in the new
        location.

        .. warning::
                Defragmenting with Linux kernel versions < 3.9 or ≥ 3.14-rc2 as
                well as with Linux stable kernel versions ≥ 3.10.31, ≥ 3.12.12 or
                ≥ 3.13.4 will break up the reflinks of COW data (for example files
                copied with :command:`cp --reflink`, snapshots or de-duplicated data).
                This may cause considerable increase of space usage depending on the
                broken up reflinks.

barrier, nobarrier
        (default: on)

        Ensure that all IO write operations make it through the device cache and are stored
        permanently when the filesystem is at its consistency checkpoint. This
        typically means that a flush command is sent to the device that will
        synchronize all pending data and ordinary metadata blocks, then writes the
        superblock and issues another flush.

        The write flushes incur a slight hit and also prevent the IO block
        scheduler to reorder requests in a more effective way. Disabling barriers gets
        rid of that penalty but will most certainly lead to a corrupted filesystem in
        case of a crash or power loss. The ordinary metadata blocks could be yet
        unwritten at the time the new superblock is stored permanently, expecting that
        the block pointers to metadata were stored permanently before.

        On a device with a volatile battery-backed write-back cache, the *nobarrier*
        option will not lead to filesystem corruption as the pending blocks are
        supposed to make it to the permanent storage.

clear_cache
        Force clearing and rebuilding of the free space cache if something
        has gone wrong.

        For free space cache *v1*, this only clears (and, unless *nospace_cache* is
        used, rebuilds) the free space cache for block groups that are modified while
        the filesystem is mounted with that option. To actually clear an entire free
        space cache *v1*, see :command:`btrfs check --clear-space-cache v1`.

        For free space cache *v2*, this clears the entire free space cache.
        To do so without requiring to mounting the filesystem, see
        :command:`btrfs check --clear-space-cache v2`.

        See also: *space_cache*.

commit=<seconds>
        (since: 3.12, default: 30)

        Set the interval of periodic transaction commit when data are synchronized
        to permanent storage. Higher interval values lead to larger amount of unwritten
        data to accumulate in memory, which has obvious consequences when the
        system crashes.  The upper bound is not forced, but a warning is
        printed if it's more than 300 seconds (5 minutes). Use with care.

        The periodic commit is not the only mechanism to do the transaction commit,
        this can also happen by explicit :command:`sync` or indirectly by other
        commands that affect the global filesystem state or internal kernel
        mechanisms that flush based on various thresholds or policies (e.g. cgroups).

compress, compress=<type[:level]>, compress-force, compress-force=<type[:level]>
        (default: off, level support since: 5.1)

        Control BTRFS file data compression.  Type may be specified as *zlib*,
        *lzo*, *zstd* or *no* (for no compression, used for remounting).  If no type
        is specified, *zlib* is used.  If *compress-force* is specified,
        then compression will always be attempted, but the data may end up uncompressed
        if the compression would make them larger.

        Both *zlib* and *zstd* (since version 5.1) expose the compression level as a
        tunable knob with higher levels trading speed and memory (*zstd*) for higher
        compression ratios. This can be set by appending a colon and the desired level.
        ZLIB accepts the range [1, 9] and ZSTD accepts [1, 15]. If no level is set,
        both currently use a default level of 3. The value 0 is an alias for the
        default level.

        Otherwise some simple heuristics are applied to detect an incompressible file.
        If the first blocks written to a file are not compressible, the whole file is
        permanently marked to skip compression. As this is too simple, the
        *compress-force* is a workaround that will compress most of the files at the
        cost of some wasted CPU cycles on failed attempts.
        Since kernel 4.15, a set of heuristic algorithms have been improved by using
        frequency sampling, repeated pattern detection and Shannon entropy calculation
        to avoid that.

        .. note::
                If compression is enabled, *nodatacow* and *nodatasum* are disabled.

datacow, nodatacow
        (default: on)

        Enable data copy-on-write for newly created files.
        *Nodatacow* implies *nodatasum*, and disables *compression*. All files created
        under *nodatacow* are also set the NOCOW file attribute (see :manref:`chattr(1)`).

        .. note::
                If *nodatacow* or *nodatasum* are enabled, compression is disabled.

        Updates in-place improve performance for workloads that do frequent overwrites,
        at the cost of potential partial writes, in case the write is interrupted
        (system crash, device failure).

datasum, nodatasum
        (default: on)

        Enable data checksumming for newly created files.
        *Datasum* implies *datacow*, i.e. the normal mode of operation. All files created
        under *nodatasum* inherit the "no checksums" property, however there's no
        corresponding file attribute (see :manref:`chattr(1)`).

        .. note::
                If *nodatacow* or *nodatasum* are enabled, compression is disabled.

        There is a slight performance gain when checksums are turned off, the
        corresponding metadata blocks holding the checksums do not need to updated.
        The cost of checksumming of the blocks in memory is much lower than the IO,
        modern CPUs feature hardware support of the checksumming algorithm.

.. duplabel:: mount-option-degraded

degraded
        (default: off)

        Allow mounts with fewer devices than the RAID profile constraints
        require.  A read-write mount (or remount) may fail when there are too many devices
        missing, for example if a stripe member is completely missing from RAID0.

        Since 4.14, the constraint checks have been improved and are verified on the
        chunk level, not at the device level. This allows degraded mounts of
        filesystems with mixed RAID profiles for data and metadata, even if the
        device number constraints would not be satisfied for some of the profiles.

        Example: metadata -- raid1, data -- single, devices -- :file:`/dev/sda`, :file:`/dev/sdb`

        Suppose the data are completely stored on *sda*, then missing *sdb* will not
        prevent the mount, even if 1 missing device would normally prevent (any)
        *single* profile to mount. In case some of the data chunks are stored on *sdb*,
        then the constraint of single/data is not satisfied and the filesystem
        cannot be mounted.

.. duplabel:: mount-option-device

device=<devicepath>
        Specify a path to a device that will be scanned for BTRFS filesystem during
        mount. This is usually done automatically by a device manager (like udev) or
        using the **btrfs device scan** command (e.g. run from the initial ramdisk). In
        cases where this is not possible the *device* mount option can help.

        .. note::
                Booting e.g. a RAID1 system may fail even if all filesystem's *device*
                paths are provided as the actual device nodes may not be discovered by the
                system at that point.

discard, discard=sync, discard=async, nodiscard
        (default: async when devices support it since 6.2, async support since: 5.6)

        Enable discarding of freed file blocks.  This is useful for SSD/NVMe
        devices, thinly provisioned LUNs, or virtual machine images; however,
        every storage layer must support discard for it to work.

        In the synchronous mode (*sync* or without option value), lack of asynchronous
        queued TRIM on the backing device TRIM can severely degrade performance,
        because a synchronous TRIM operation will be attempted instead. Queued TRIM
        requires SATA devices with chipsets revision newer than 3.1 and devices.

        The asynchronous mode (*async*) gathers extents in larger chunks before sending
        them to the devices for TRIM. The overhead and performance impact should be
        negligible compared to the previous mode and it's supposed to be the preferred
        mode if needed.

        If it is not necessary to immediately discard freed blocks, then the :command:`fstrim`
        tool can be used to discard all free blocks in a batch. Scheduling a TRIM
        during a period of low system activity will prevent latent interference with
        the performance of other operations. Also, a device may ignore the TRIM command
        if the range is too small, so running a batch discard has a greater probability
        of actually discarding the blocks.

enospc_debug, noenospc_debug
        (default: off)

        Enable verbose output for some ENOSPC conditions. It's safe to use but can
        be noisy if the system reaches near-full state.

fatal_errors=<action>
        (since: 3.4, default: bug)

        Action to take when encountering a fatal error.

        bug
                *BUG()* on a fatal error, the system will stay in the crashed state and may be
                still partially usable, but reboot is required for full operation
        panic
                *panic()* on a fatal error, depending on other system configuration, this may
                be followed by a reboot. Please refer to the documentation of kernel boot
                parameters, e.g. *panic*, *oops* or *crashkernel*.

flushoncommit, noflushoncommit
        (default: off)

        This option forces any data dirtied by a write in a prior transaction to commit
        as part of the current commit, effectively a full filesystem sync.

        This makes the committed state a fully consistent view of the file system from
        the application's perspective (i.e. it includes all completed file system
        operations). This was previously the behavior only when a snapshot was
        created.

        When off, the filesystem is consistent but buffered writes may last more than
        one transaction commit.

fragment=<type>
        (depends on compile-time option CONFIG_BTRFS_DEBUG, since: 4.4, default: off)

        A debugging helper to intentionally fragment given *type* of block groups. The
        type can be *data*, *metadata* or *all*. This mount option should not be used
        outside of debugging environments and is not recognized if the kernel config
        option *CONFIG_BTRFS_DEBUG* is not enabled.

nologreplay
        (default: off, even read-only)

        The tree-log contains pending updates to the filesystem until the full commit.
        The log is replayed on next mount, this can be disabled by this option.  See
        also *treelog*.  Note that *nologreplay* is the same as *norecovery*.

        .. warning::
                Currently, the tree log is replayed even with a read-only mount! To
                disable that behaviour, mount also with *nologreplay*.

max_inline=<bytes>
        (default: min(2048, page size) )

        Specify the maximum amount of space, that can be inlined in
        a metadata b-tree leaf.  The value is specified in bytes, optionally
        with a K suffix (case insensitive).  In practice, this value
        is limited by the filesystem block size (named *sectorsize* at mkfs time),
        and memory page size of the system. In case of sectorsize limit, there's
        some space unavailable due to b-tree leaf headers.  For example, a 4KiB
        sectorsize, maximum size of inline data is about 3900 bytes.

        Inlining can be completely turned off by specifying 0. This will increase data
        block slack if file sizes are much smaller than block size but will reduce
        metadata consumption in return.

        .. note::
                The default value has changed to 2048 in kernel 4.6.

metadata_ratio=<value>
        (default: 0, internal logic)

        Specifies that 1 metadata chunk should be allocated after every *value* data
        chunks. Default behaviour depends on internal logic, some percent of unused
        metadata space is attempted to be maintained but is not always possible if
        there's not enough space left for chunk allocation. The option could be useful to
        override the internal logic in favor of the metadata allocation if the expected
        workload is supposed to be metadata intense (snapshots, reflinks, xattrs,
        inlined files).

norecovery
        (since: 4.5, default: off)

        Do not attempt any data recovery at mount time. This will disable *logreplay*
        and avoids other write operations. Note that this option is the same as
        *nologreplay*.

        .. note::
                The opposite option *recovery* used to have different meaning but was
                changed for consistency with other filesystems, where *norecovery* is used for
                skipping log replay. BTRFS does the same and in general will try to avoid any
                write operations.

.. duplabel:: mount-option-rescan-uuid-tree

rescan_uuid_tree
        (since: 3.12, default: off)

        Force check and rebuild procedure of the UUID tree. This should not
        normally be needed. Alternatively the tree can be cleared from
        userspace by command :ref:`btrfs rescue clear-uuid-tree<man-rescue-clear-uuid-tree>`
        and then it will be automatically rebuilt in kernel (the mount option
        is not needed in that case).

rescue
        (since: 5.9)

        Modes allowing mount with damaged filesystem structures, all requires
	the filesystem to be mounted read-only and doesn't allow remount to read-write.
        This is supposed to provide unified and more fine grained tuning of
        errors that affect filesystem operation.

        * *usebackuproot* (since 5.9)

	  Try to use backup root slots inside super block.
	  Replaces standalone option *usebackuproot*

        * *nologreplay* (since 5.9)

	  Do not replay any dirty logs.
	  Replaces standalone option *nologreplay*

        * *ignorebadroots*, *ibadroots* (since: 5.11)

	  Ignore bad tree roots, greatly improve the chance for data salvage.

        * *ignoredatacsums*, *idatacsums* (since: 5.11)

	  Ignore data checksum verification.

	* *ignoremetacsums*, *imetacsums* (since 6.12)

	  Ignore metadata checksum verification, useful for interrupted checksum conversion.

        * *all* (since: 5.9)

	  Enable all supported rescue options.

skip_balance
        (since: 3.3, default: off)

        Skip automatic resume of an interrupted balance operation. The operation can
        later be resumed with :command:`btrfs balance resume`, or the paused state can be
        removed with :command:`btrfs balance cancel`. The default behaviour is to resume an
        interrupted balance immediately after the filesystem is mounted.

space_cache, space_cache=<version>, nospace_cache
        (*nospace_cache* since: 3.2, *space_cache=v1* and *space_cache=v2* since 4.5, default: *space_cache=v2*)

        Options to control the free space cache. The free space cache greatly improves
        performance when reading block group free space into memory. However, managing
        the space cache consumes some resources, including a small amount of disk
        space.

        There are two implementations of the free space cache. The original
        one, referred to as *v1*, used to be a safe default but has been
        superseded by *v2*.  The *v1* space cache can be disabled at mount time
        with *nospace_cache* without clearing.

        On very large filesystems (many terabytes) and certain workloads, the
        performance of the *v1* space cache may degrade drastically. The *v2*
        implementation, which adds a new b-tree called the free space tree, addresses
        this issue. Once enabled, the *v2* space cache will always be used and cannot
        be disabled unless it is cleared. Use *clear_cache,space_cache=v1* or
        *clear_cache,nospace_cache* to do so. If *v2* is enabled, and *v1* space
        cache will be cleared (at the first mount) and kernels without *v2*
        support will only be able to mount the filesystem in read-only mode.
        On an unmounted filesystem the caches (both versions) can be cleared by
        "btrfs check --clear-space-cache".

        The :doc:`btrfs-check` and `:doc:`mkfs.btrfs` commands have full *v2* free space
        cache support since v4.19.

        If a version is not explicitly specified, the default implementation will be
        chosen, which is *v2*.

ssd, ssd_spread, nossd, nossd_spread
        (default: SSD autodetected)

        Options to control SSD allocation schemes.  By default, BTRFS will
        enable or disable SSD optimizations depending on status of a device with
        respect to rotational or non-rotational type. This is determined by the
        contents of :file:`/sys/block/DEV/queue/rotational`). If it is 0, the
        *ssd* option is turned on.  The option *nossd* will disable the
        autodetection.

        The optimizations make use of the absence of the seek penalty that's inherent
        for the rotational devices. The blocks can be typically written faster and
        are not offloaded to separate threads.

        .. note::
                Since 4.14, the block layout optimizations have been dropped. This used
                to help with first generations of SSD devices. Their FTL (flash translation
                layer) was not effective and the optimization was supposed to improve the wear
                by better aligning blocks. This is no longer true with modern SSD devices and
                the optimization had no real benefit. Furthermore it caused increased
                fragmentation. The layout tuning has been kept intact for the option
                *ssd_spread*.

        The *ssd_spread* mount option attempts to allocate into bigger and aligned
        chunks of unused space, and may perform better on low-end SSDs.  *ssd_spread*
        implies *ssd*, enabling all other SSD heuristics as well. The option *nossd*
        will disable all SSD options while *nossd_spread* only disables *ssd_spread*.

subvol=<path>
        Mount subvolume from *path* rather than the toplevel subvolume. The
        *path* is always treated as relative to the toplevel subvolume.
        This mount option overrides the default subvolume set for the given filesystem.

subvolid=<subvolid>
        Mount subvolume specified by a *subvolid* number rather than the toplevel
        subvolume.  You can use :command:`btrfs subvolume list` of :command:`btrfs subvolume show` to see
        subvolume ID numbers.
        This mount option overrides the default subvolume set for the given filesystem.

        .. note::
                If both *subvolid* and *subvol* are specified, they must point at the
                same subvolume, otherwise the mount will fail.

thread_pool=<number>
        (default: min(NRCPUS + 2, 8) )

        The number of worker threads to start. NRCPUS is number of on-line CPUs
        detected at the time of mount. Small number leads to less parallelism in
        processing data and metadata, higher numbers could lead to a performance hit
        due to increased locking contention, process scheduling, cache-line bouncing or
        costly data transfers between local CPU memories.

treelog, notreelog
        (default: on)

        Enable the tree logging used for *fsync* and *O_SYNC* writes. The tree log
        stores changes without the need of a full filesystem sync. The log operations
        are flushed at sync and transaction commit. If the system crashes between two
        such syncs, the pending tree log operations are replayed during mount.

        .. warning::
                Currently, the tree log is replayed even with a read-only mount! To
                disable that behaviour, also mount with *nologreplay*.

        The tree log could contain new files/directories, these would not exist on
        a mounted filesystem if the log is not replayed.

usebackuproot
        (since: 4.6, default: off)

        Enable autorecovery attempts if a bad tree root is found at mount time.
        Currently this scans a backup list of several previous tree roots and tries to
        use the first readable. This can be used with read-only mounts as well.

        .. note::
                This option has replaced *recovery* which has been deprecated.

user_subvol_rm_allowed
        (default: off)

        Allow subvolumes to be deleted by their respective owner. Otherwise, only the
        root user can do that.

        .. note::
                Historically, any user could create a snapshot even if he was not owner
                of the source subvolume, the subvolume deletion has been restricted for that
                reason. The subvolume creation has been restricted but this mount option is
                still required. This is a usability issue.
                Since 4.18, the :manref:`rmdir(2)` syscall can delete an empty subvolume just like an
                ordinary directory. Whether this is possible can be detected at runtime, see
                *rmdir_subvol* feature in *FILESYSTEM FEATURES*.

DEPRECATED MOUNT OPTIONS
^^^^^^^^^^^^^^^^^^^^^^^^

List of mount options that have been removed, kept for backward compatibility.

recovery
        (since: 3.2, default: off, deprecated since: 4.5)

        .. note::
                This option has been replaced by *usebackuproot* and should not be used
                but will work on 4.5+ kernels.

inode_cache, noinode_cache
        (removed in: 5.11, since: 3.0, default: off)

        .. note::
                The functionality has been removed in 5.11, any stale data created by
                previous use of the *inode_cache* option can be removed by
                :ref:`btrfs rescue clear-ino-cache<man-rescue-clear-ino-cache>`.

check_int, check_int_data, check_int_print_mask=<value>
        (removed in: 6.7, since: 3.0, default: off)

        These debugging options control the behavior of the integrity checking
        module (the BTRFS_FS_CHECK_INTEGRITY config option required). The main goal is
        to verify that all blocks from a given transaction period are properly linked.

        *check_int* enables the integrity checker module, which examines all
        block write requests to ensure on-disk consistency, at a large
        memory and CPU cost.

        *check_int_data* includes extent data in the integrity checks, and
        implies the *check_int* option.

        *check_int_print_mask* takes a bit mask of BTRFSIC_PRINT_MASK_* values
        as defined in *fs/btrfs/check-integrity.c*, to control the integrity
        checker module behavior.

        See comments at the top of *fs/btrfs/check-integrity.c*
        for more information.


NOTES ON GENERIC MOUNT OPTIONS
^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

Some of the general mount options from :manref:`mount(8)` that affect BTRFS and are
worth mentioning.

.. duplabel:: mount-options-generic

context
        The context refers to the SELinux contexts and policy definitions passed
        as mount options. This works properly since version v6.8 (because the
        mount option parser of BTRFS was ported to new API that also understood
        the options).

noatime
        under read intensive work-loads, specifying *noatime* significantly improves
        performance because no new access time information needs to be written. Without
        this option, the default is *relatime*, which only reduces the number of
        inode atime updates in comparison to the traditional *strictatime*. The worst
        case for atime updates under *relatime* occurs when many files are read whose
        atime is older than 24 h and which are freshly snapshotted. In that case the
        atime is updated and COW happens - for each file - in bulk. See also
        https://lwn.net/Articles/499293/ - *Atime and btrfs: a bad combination? (LWN, 2012-05-31)*.

        Note that *noatime* may break applications that rely on atime uptimes like
        the venerable Mutt (unless you use *maildir* mailboxes).



Bootloaders
-----------

GRUB2 (https://www.gnu.org/software/grub) has the most advanced support of
booting from BTRFS with respect to features.

U-Boot (https://www.denx.de/wiki/U-Boot/) has decent support for booting but
not all BTRFS features are implemented, check the documentation.

In general, the first 1MiB on each device is unused with the exception of
primary superblock that is on the offset 64KiB and spans 4KiB. The rest can be
freely used by bootloaders or for other system information. Note that booting
from a filesystem on :doc:`zoned device<Zoned-mode>` is not supported.


.. _administration-limits:

Filesystem limits
-----------------

maximum file name length
        255

        This limit is imposed by Linux VFS, the structures of BTRFS could store
        larger file names.

maximum symlink target length
        depends on the *nodesize* value, for 4KiB it's 3949 bytes, for larger nodesize
        it's 4095 due to the system limit PATH_MAX

        The symlink target may not be a valid path, i.e. the path name components
        can exceed the limits (NAME_MAX), there's no content validation at :manref:`symlink(3)`
        creation.

maximum number of inodes
        2\ :sup:`64` but depends on the available metadata space as the inodes are created
        dynamically

        Each subvolume is an independent namespace of inodes and thus their
        numbers, so the limit is per subvolume, not for the whole filesystem.

inode numbers
        minimum number: 256 (for subvolumes), regular files and directories: 257,
        maximum number: (2\ :sup:`64` - 256)

        The inode numbers that can be assigned to user created files are from
        the whole 64bit space except first 256 and last 256 in that range that
        are reserved for internal b-tree identifiers.

maximum file length
        inherent limit of BTRFS is 2\ :sup:`64` (16 EiB) but the practical
        limit of Linux VFS is 2\ :sup:`63` (8 EiB)

maximum number of subvolumes
        the subvolume ids can go up to 2\ :sup:`48` but the number of actual subvolumes
        depends on the available metadata space

        The space consumed by all subvolume metadata includes bookkeeping of
        shared extents can be large (MiB, GiB). The range is not the full 64bit
        range because of qgroups that use the upper 16 bits for another
        purposes.

maximum number of hardlinks of a file in a directory
        65536 when the *extref* feature is turned on during mkfs (default), roughly
        100 otherwise and depends on file name length that fits into one metadata node

minimum filesystem size
        the minimal size of each device depends on the *mixed-bg* feature, without that
        (the default) it's about 109MiB, with mixed-bg it's is 16MiB


.. _administration-flexibility:

Flexibility
-----------

The underlying design of BTRFS data structures allows a lot of flexibility and
making changes after filesystem creation, like resizing, adding/removing space
or enabling some features on-the-fly.

* **dynamic inode creation** -- there's no fixed space or tables for tracking
  inodes so the number of inodes that can be created is bounded by the metadata
  space and its utilization

* **block group profile change on-the-fly** -- the block group profiles can be
  changed on a mounted filesystem by running the balance operation and
  specifying the conversion filters (see :doc:`balance<Balance>`)

* **resize** -- the space occupied by the filesystem on each device can be
  resized up (grow) or down (shrink) as long as the amount of data can be still
  contained on the device

* **device management** -- devices can be added, removed or replaced without
  requiring recreating the filesystem (mkfs), new redundancy options available
  on more devices can be also utilized by rebalancing


Sysfs
-----

Btrfs has a sysfs interface to provide extra knobs.

The top level path is :file:`/sys/fs/btrfs/`, and the main directory layout is the following:

=============================  ===================================  ========
Relative Path                  Description                          Version
=============================  ===================================  ========
features/                      All supported features               3.14
<UUID>/                        Mounted fs UUID                      3.14
<UUID>/allocation/             Space allocation info                3.14
<UUID>/bdi/                    Backing device info (writeback)      5.9
<UUID>/devices/<DEVID>/        Symlink to each block device sysfs   5.6
<UUID>/devinfo/<DEVID>/        Btrfs specific info for each device  5.6
<UUID>/discard/                Discard stats and tunables           6.1
<UUID>/features/               Features of the filesystem           3.14
<UUID>/qgroups/                Global qgroup info                   5.9
<UUID>/qgroups/<LEVEL>_<ID>/   Info for each qgroup                 5.9
=============================  ===================================  ========

For :file:`/sys/fs/btrfs/features/` directory, each file means a supported feature
of the current kernel. Most files have value 0. Otherwise it depends on the file,
value *1* typically means the feature can be turned on a mounted filesystem.

For :file:`/sys/fs/btrfs/<UUID>/features/` directory, each file means an enabled
feature on the mounted filesystem.

The features share the same name in section
:ref:`FILESYSTEM FEATURES<man-btrfs5-filesystem-features>`.

UUID
^^^^

Files in :file:`/sys/fs/btrfs/<UUID>/` directory are:

bg_reclaim_threshold
        (RW, since: 5.19)

        Used space percentage of total device space to start auto block group claim.
        Mostly for zoned devices.

checksum
        (RO, since: 5.5)

        The checksum used for the mounted filesystem.
        This includes both the checksum type (see section
        :ref:`CHECKSUM ALGORITHMS<man-btrfs5-checksum-algorithms>`)
        and the implemented driver (mostly shows if it's hardware accelerated).

clone_alignment
        (RO, since: 3.16)

        The bytes alignment for *clone* and *dedupe* ioctls.

commit_stats
        (RW, since: 6.0)

        The performance statistics for btrfs transaction commit since the first
        mount. Mostly for debugging purposes.

        Writing into this file will reset the maximum commit duration
        (*max_commit_ms*) to 0. The file looks like:

        .. code-block:: none

                commits 70649
                last_commit_ms 2
                max_commit_ms 131
                total_commit_ms 170840

        * *commits* - number of transaction commits since the first mount
        * *last_commit_ms* - duration in milliseconds of the last commit
        * *max_commit_ms* - maximum time a transaction commit took since first mount or last reset
        * *total_commit_ms* - sum of all transaction commit times

exclusive_operation
        (RO, since: 5.10)

        Shows the running exclusive operation.
        Check section
        :ref:`FILESYSTEM EXCLUSIVE OPERATIONS<man-btrfs5-filesystem-exclusive-operations>`
        for details.

generation
        (RO, since: 5.11)

        Show the generation of the mounted filesystem.

label
        (RW, since: 3.14)

        Show the current label of the mounted filesystem.

metadata_uuid
        (RO, since: 5.0)

        Shows the metadata UUID of the mounted filesystem.
        Check `metadata_uuid` feature for more details.

nodesize
        (RO, since: 3.14)

        Show the nodesize of the mounted filesystem.

quota_override
        (RW, since: 4.13)

        Shows the current quota override status.
        0 means no quota override.
        1 means quota override, quota can ignore the existing limit settings.

read_policy
        (RW, since: 5.11)

        Shows the current balance policy for reads.
        Currently only ``pid`` (balance using the process id (pid) value) is
        supported. More balancing policies are available in experimental
        build, namely round-robin.

sectorsize
        (RO, since: 3.14)

        Shows the sectorsize of the mounted filesystem.

temp_fsid
        (RO, since 6.7)

        Indicate that this filesystem got assigned a temporary FSID at mount time,
        making possible to mount devices with the same FSID.

UUID/allocations
^^^^^^^^^^^^^^^^

Files and directories in :file:`/sys/fs/btrfs/<UUID>/allocations` directory are:

global_rsv_reserved
        (RO, since: 3.14)

        The used bytes of the global reservation.

global_rsv_size
        (RO, since: 3.14)

        The total size of the global reservation.

`data/`, `metadata/` and `system/` directories
        (RO, since: 5.14)

        Space info accounting for the 3 block group types.

UUID/allocations/{data,metadata,system}
"""""""""""""""""""""""""""""""""""""""

Files in :file:`/sys/fs/btrfs/<UUID>/allocations/{data,metadata,system}` directory are:

bg_reclaim_threshold
        (RW, since: 5.19)

        Reclaimable space percentage of block group's size (excluding
        permanently unusable space) to reclaim the block group.
        Can be used on regular or zoned devices.

bytes_*
        (RO)

        Values of the corresponding data structures for the given block group
        type and profile that are used internally and may change rapidly depending
        on the load.

        Complete list: bytes_may_use, bytes_pinned, bytes_readonly,
        bytes_reserved, bytes_used, bytes_zone_unusable

chunk_size
        (RW, since: 6.0)

        Shows the chunk size. Can be changed for data and metadata (independently)
        and cannot be set for system block group type.
        Cannot be set for zoned devices as it depends on the fixed device zone size.
        Upper bound is 10% of the filesystem size, the value must be multiple of 256MiB
        and greater than 0.

size_classes
        (RO, since: 6.3)

        Numbers of block groups of a given classes based on heuristics that
        measure extent length, age and fragmentation.

        .. code-block:: none

                none 136
                small 374
                medium 282
                large 93

UUID/bdi
^^^^^^^^

Symlink to the sysfs directory of the backing device info (BDI), which is
related to writeback process and infrastructure.

UUID/devices
^^^^^^^^^^^^

Files in :file:`/sys/fs/btrfs/<UUID>/devices` directory are symlinks named
after device nodes (e.g. sda, dm-0) and pointing to their sysfs directory.

UUID/devinfo
^^^^^^^^^^^^

The directory contains subdirectories named after device ids (numeric values). Each
subdirectory has information about the device of the given *devid*.

UUID/devinfo/DEVID
""""""""""""""""""

Files in :file:`/sys/fs/btrfs/<UUID>/devinfo/<DEVID>` directory are:

error_stats:
        (RO, since: 5.14)

        Shows device stats of this device, same as :command:`btrfs device stats` (:doc:`btrfs-device`).

        .. code-block:: none

                write_errs 0
                read_errs 0
                flush_errs 0
                corruption_errs 0
                generation_errs 0

fsid:
        (RO, since: 5.17)

        Shows the fsid which the device belongs to.
        It can be different than the ``UUID`` if it's a seed device.

in_fs_metadata
        (RO, since: 5.6)

        Shows whether we have found the device.
        Should always be 1, as if this turns to 0, the :file:`DEVID` directory
        would get removed automatically.

missing
        (RO, since: 5.6)

        Shows whether the device is considered missing by the kernel module.

replace_target
        (RO, since: 5.6)

        Shows whether the device is the replace target.
        If no device replace is running, this value is 0.

scrub_speed_max
        (RW, since: 5.14)

        Shows the scrub speed limit for this device. The unit is Bytes/s.
        0 means no limit. The value can be set but is not persistent.

writeable
        (RO, since: 5.6)

        Show if the device is writeable.

UUID/qgroups
^^^^^^^^^^^^

Files in :file:`/sys/fs/btrfs/<UUID>/qgroups/` directory are:

enabled
        (RO, since: 6.1)

        Shows if qgroup is enabled.
        Also, if qgroup is disabled, the :file:`qgroups` directory will
        be removed automatically.

inconsistent
        (RO, since: 6.1)

        Shows if the qgroup numbers are inconsistent.
        If 1, it's recommended to do a qgroup rescan.

drop_subtree_threshold
        (RW, since: 6.1)

        Shows the subtree drop threshold to automatically mark qgroup inconsistent.

        When dropping large subvolumes with qgroup enabled, there would be a huge
        load for qgroup accounting.
        If we have a subtree whose level is larger than or equal to this value,
        we will not trigger qgroup account at all, but mark qgroup inconsistent to
        avoid the huge workload.

        Default value is 3, which means that trees of low height will be accounted
        properly as this is sufficiently fast. The value was 8 until 6.13 where
        no subtree drop can trigger qgroup rescan making it less useful.

        Lower value can reduce qgroup workload, at the cost of extra qgroup rescan
        to re-calculate the numbers.

UUID/qgroups/LEVEL_ID
"""""""""""""""""""""

Files in each :file:`/sys/fs/btrfs/<UUID>/qgroups/<LEVEL>_<ID>/` directory are:

exclusive
        (RO, since: 5.9)

        Shows the exclusively owned bytes of the qgroup.

limit_flags
        (RO, since: 5.9)

        Shows the numeric value of the limit flags.
        If 0, means no limit implied.

max_exclusive
        (RO, since: 5.9)

        Shows the limits on exclusively owned bytes.

max_referenced
        (RO, since: 5.9)

        Shows the limits on referenced bytes.

referenced
        (RO, since: 5.9)

        Shows the referenced bytes of the qgroup.

rsv_data
        (RO, since: 5.9)

        Shows the reserved bytes for data.

rsv_meta_pertrans
        (RO, since: 5.9)

        Shows the reserved bytes for per transaction metadata.

rsv_meta_prealloc
        (RO, since: 5.9)

        Shows the reserved bytes for preallocated metadata.

UUID/discard
^^^^^^^^^^^^

Files in :file:`/sys/fs/btrfs/<UUID>/discard/` directory are:

discardable_bytes
        (RO, since: 6.1)

        Shows amount of bytes that can be discarded in the async discard and
        nodiscard mode.

discardable_extents
        (RO, since: 6.1)

        Shows number of extents to be discarded in the async discard and
        nodiscard mode.

discard_bitmap_bytes
        (RO, since: 6.1)

        Shows amount of discarded bytes from data tracked as bitmaps.

discard_extent_bytes
        (RO, since: 6.1)

        Shows amount of discarded extents from data tracked as bitmaps.

discard_bytes_saved
        (RO, since: 6.1)

        Shows the amount of bytes that were reallocated without being discarded.

kbps_limit
        (RW, since: 6.1)

        Tunable limit of kilobytes per second issued as discard IO in the async
        discard mode.

iops_limit
        (RW, since: 6.1)

        Tunable limit of number of discard IO operations to be issued in the
        async discard mode.

max_discard_size
        (RW, since: 6.1)

        Tunable limit for size of one IO discard request.

