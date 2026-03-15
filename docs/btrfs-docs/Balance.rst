Balance
=======

The primary purpose of the balance feature is to spread block groups across
all devices so they match constraints defined by the respective profiles. See
:doc:`mkfs.btrfs` section :ref:`PROFILES<man-mkfs-profiles>`
for more details.
The scope of the balancing process can be further tuned by use of filters that
can select the block groups to process. Balance works only on a mounted
filesystem.  Extent sharing is preserved and reflinks are not broken.
Files are not defragmented nor recompressed, file extents are preserved
but the physical location on devices will change.

The balance operation is cancellable by the user. The on-disk state of the
filesystem is always consistent so an unexpected interruption (e.g. system crash,
reboot) does not corrupt the filesystem. The progress of the balance operation
is temporarily stored as an internal state and will be resumed upon mount,
unless the mount option *skip_balance* is specified.

.. warning::
   Running balance without filters will take a lot of time as it basically move
   data/metadata from the whole filesystem and needs to update all block
   pointers.

The filters can be used to perform following actions:

- convert block group profiles (filter *convert*)
- make block group usage more compact  (filter *usage*)
- perform actions only on a given device (filters *devid*, *drange*)

The filters can be applied to a combination of block group types (data,
metadata, system). Note that changing only the *system* type needs the force
option. Otherwise *system* gets automatically converted whenever *metadata*
profile is converted.

When metadata redundancy is reduced (e.g. from RAID1 to single) the force option
is also required and it is noted in system log.

.. note::
   The balance operation needs enough work space, i.e. space that is completely
   unused in the filesystem, otherwise this may lead to ENOSPC reports.  See
   the section *ENOSPC* for more details.

Compatibility
-------------

.. note::

   The balance subcommand also exists under the :command:`btrfs filesystem` namespace.
   This still works for backward compatibility but is deprecated and should not
   be used any more.

.. note::
   A short syntax :command:`btrfs balance <path>` works due to backward compatibility
   but is deprecated and should not be used any more. Use :command:`btrfs balance start`
   command instead.

Performance implications
------------------------

Balancing operations are very IO intensive and can also be quite CPU intensive,
impacting other ongoing filesystem operations. Typically large amounts of data
are copied from one location to another, with corresponding metadata updates.

Depending upon the block group layout, it can also be seek heavy. Performance
on rotational devices is noticeably worse compared to SSDs or fast arrays.


Filters
-------

From kernel 3.3 onwards, BTRFS balance can limit its action to a subset of the
whole filesystem, and can be used to change the replication configuration (e.g.
convert data from ``single`` to ``RAID1``).

Balance can be limited to a block group profile with the following options:

* ``-d`` for data block groups
* ``-m`` for metadata block groups (also implicitly applies to *-s*)
* ``-s`` for system block groups

The options have an optional parameter which means that the parameter must start
right after the option without a space (this is mandatory getopt syntax), like
``-dusage=10``. Options for all block group types can be specified in one command.

A filter has the following structure: ``filter[=params][,filter=...]``

To combine multiple filters use ``,``, without spaces. Example: ``-dconvert=raid1,soft``

BTRFS can have different profiles on a single device or the same profile on
multiple device.

The main reason why you want to have different profiles for data and metadata
is to provide additional protection of the filesystem's metadata when devices fail,
since a single sector of unrecoverable metadata will break the filesystem,
while a single sector of lost data can be trivially recovered by deleting the broken file.

Before changing profiles, make sure there is enough unallocated space on
existing drives to create new metadata block groups (for filesystems
over 50GiB, this is ``1GB * (number_of_devices + 2))``.

Default profiles on BTRFS are:

* data: ``single``
* metadata:
        * single devices: ``dup``
        * multiple devices: ``raid1``


The available filter types are:

Filter types
^^^^^^^^^^^^

profiles=<profiles>
        Balances only block groups with the given profiles. Parameters
        are a list of profile names separated by ``|`` (pipe).

usage=<percent>, usage=<range>
        Balances only block groups with usage under the given percentage. The
        value of 0 is allowed and will clean up completely unused block groups, this
        should not require any new work space allocated. You may want to use *usage=0*
        in case balance is returning ENOSPC and your filesystem is not too full.

        The argument may be a single value or a range. The single value ``N`` means *at
        most N percent used*, equivalent to ``..N`` range syntax. Kernels prior to 4.4
        accept only the single value format.
        The minimum range boundary is inclusive, maximum is exclusive.

devid=<id>
        Balances only block groups which have at least one chunk on the given
        device. To list devices with ids use :command:`btrfs filesystem show`.

drange=<range>
        Balance only block groups which overlap with the given byte range on any
        device. Use in conjunction with ``devid`` to filter on a specific device. The
        parameter is a range specified as ``start..end``.

vrange=<range>
        Balance only block groups which overlap with the given byte range in the
        filesystem's internal virtual address space. This is the address space that
        most reports from btrfs in the kernel log use. The parameter is a range
        specified as ``start..end``.

convert=<profile>
        Convert each selected block group to the given profile name identified by
        parameters.

        .. note::
                Starting with kernel 4.5, the ``data`` chunks can be converted to/from the
                ``DUP`` profile on a single device.

                Starting with kernel 4.6, all profiles can be converted to/from ``DUP`` on
                multi-device filesystems.

	.. warning::
                Bad or missing device are not detected immediately during
                runtime and this depends on some later event like failed write
                or failed transaction commit. If there's a known failing
                device, or a device deleted by :file:`/sys/block/<dev>/device/delete` interface,
                the device will be still accessed and written to.

                In such case, one should not convert to a profile with lower
                redundancy (e.g. from *RAID1* to *SINGLE*),
                as attempts to create new chunks on the new devices will cause
                various problems.

                The proper action is to use :command:`btrfs replace` or
                :command:`btrfs device remove` to handle the failing/missing
                device first. Then convert will work with all devices
                correctly.

limit=<number>, limit=<range>
        Process only given number of chunks, after all filters are applied. This can be
        used to specifically target a chunk in connection with other filters (``drange``,
        ``vrange``) or just simply limit the amount of work done by a single balance run.

        The argument may be a single value or a range. The single value ``N`` means *at
        most N chunks*, equivalent to ``..N`` range syntax. Kernels prior to 4.4 accept
        only the single value format.  The range minimum and maximum are inclusive.

stripes=<range>
        Balance only block groups which have the given number of stripes. The parameter
        is a range specified as ``start..end``. Makes sense for block group profiles that
        utilize striping, i.e. RAID0/10/5/6.  The range minimum and maximum are
        inclusive.

soft
        Takes no parameters. Only has meaning when converting between profiles, or
        When doing convert from one profile to another and soft mode is on,
        chunks that already have the target profile are left untouched.
        This is useful e.g. when half of the filesystem was converted earlier but got
        cancelled.

        The soft mode switch is (like every other filter) per-type.
        For example, this means that we can convert metadata chunks the "hard" way
        while converting data chunks selectively with soft switch.

Profile names, used in ``profiles`` and ``convert`` are one of:

* ``raid0``
* ``raid1``
* ``raid1c3``
* ``raid1c4``
* ``raid10``
* ``raid5``
* ``raid6``
* ``dup``
* ``single``

The mixed data/metadata profiles can be converted in the same way, but conversion
between mixed and non-mixed is not implemented. For the constraints of the
profiles please refer to :doc:`mkfs.btrfs` section
:ref:`PROFILES<man-mkfs-profiles>`.


Examples
--------

Adding new device
^^^^^^^^^^^^^^^^^

The unallocated space requirements depend on the selected storage
profiles.  The requirements for the storage profile must be met for the
selected for both data and metadata (e.g. if you have single data and
RAID1 metadata, the stricter RAID1 requirements must be met or the
filesystem may run out of metadata space and go read-only).

Before adding a drive, make sure there is enough unallocated space on
existing drives to create new metadata block groups (for filesystems
over 50GB, this is `1GB * (number_of_devices + 2))`.

If using a striped profile (`raid0`, `raid10`, `raid5`, or `raid6`), then do a
full data balance of all data after adding a drive.  If adding multiple
drives at once, do a full data balance after adding the last one.

.. code-block:: bash

        btrfs balance start -v --full-balance mnt/

If the balance is interrupted, it can be restarted using the *stripes*
filter (i.e. `-dstripes=1..N` where *N* is the previous size of the array
before the new device was added) as long as all devices are the same size.
If the device sizes are different, a specialized userspace balance tool
is required.  The data balance must be completed before adding any new
devices or increasing the size of existing ones.

.. code-block:: bash

        # For going from 4 disk to 5 disks, in Raid 5
        btrfs balance start -v -dstripes=1..4 mnt/

If you are not using a striped profile now, but intend to convert to a
striped profile in the future, always perform a full data balance after
adding drives or replacing existing drives with larger ones.  The stock
*btrfs balance* tool cannot cope with special cases on filesystems with
striped raid profiles, and will paint itself into a corner that will
require custom userspace balancing tools to recover if you try.

To watch one can use the following:

.. code-block:: bash

        watch "btrfs filesystem usage -T mnt/; btrfs balance status mnt/"

Convert RAID1 after mkfs with defaults
^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

If you forgot to set the block group profile when creating the volume, run the
following command:

.. code-block:: bash

        btrfs balance start -v convert=raid1,soft mnt/

This will convert all remaining profiles that are not yet *raid1*.

Convert data to RAID10 with RAID1C4 for metadata
^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^

If you a have multi device setup, or you'd like to have different profiles on a
single disk, e.g. *RAID10* for data and *RAID1C4* for metadata and system:

.. code-block:: bash

        btrfs balance start -v -mconvert=raid1C4,soft -dconvert=raid10,soft mnt/

Compact under used chunks
^^^^^^^^^^^^^^^^^^^^^^^^^

If the data chunks are not balanced and used only partially, the *usage* filter
can be used to make them more compact:

.. code-block:: bash

        btrfs balance start -v -dusage=10 mnt/

If the percent starts from a small number, like 5 or 10, the chunks will be
processed relatively quickly and will make more space available. Increasing the
percentage can then make more chunks compact by relocating the data.

Chunks utilized up to 50% can be relocated to other chunks while still freeing
the space. With utilization higher than 50% the chunks will be basically only
moved on the devices. The actual chunk layout may help to coalesce the free
space but this is a secondary effect.

.. code-block:: bash

        for USAGE in {10..50..10} do
            btrfs balance start -v -dusage=$USAGE mnt/
        done

Fix incomplete balance
^^^^^^^^^^^^^^^^^^^^^^

If the balance is interrupted (due to reboot or cancelled) during conversion to
RAID1. The following command will skip all RAID1 chunks that have been already
converted and continue with what's left to convert. Note that an interrupted
conversion may leave the last chunk under utilized.

.. code-block:: bash

        btrfs balance start convert=raid1,soft mnt/

