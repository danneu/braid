def _dm_delay_names(names):
    if isinstance(names, str):
        return [names]
    return names


def dm_delay_table(
    node,
    name,
    *,
    read_delay_ms=0,
    write_delay_ms=0,
    flush_delay_ms=0,
):
    raw = f"/dev/disk/by-id/virtio-{name}"
    sectors = node.succeed(f"blockdev --getsz {raw}").strip()
    return (
        f"0 {sectors} delay "
        f"{raw} 0 {read_delay_ms} "
        f"{raw} 0 {write_delay_ms} "
        f"{raw} 0 {flush_delay_ms}"
    )


def dm_delay_create(node, name, *, by_id_symlink=True):
    node.succeed("modprobe dm-delay")
    mapper = f"{name}-delay"
    node.succeed(
        f"dmsetup create {mapper} --table '"
        f"{dm_delay_table(node, name)}'"
    )
    if by_id_symlink:
        node.succeed(
            f"ln -sfn /dev/mapper/{mapper} "
            f"/dev/disk/by-id/braid-test-{name}-delay"
        )


def dm_delay_activate(
    node,
    names,
    *,
    read_delay_ms=0,
    write_delay_ms=0,
    flush_delay_ms=0,
):
    for name in _dm_delay_names(names):
        mapper = f"{name}-delay"
        table = dm_delay_table(
            node,
            name,
            read_delay_ms=read_delay_ms,
            write_delay_ms=write_delay_ms,
            flush_delay_ms=flush_delay_ms,
        )
        node.succeed(f"dmsetup suspend {mapper}")
        node.succeed(f"dmsetup reload {mapper} --table '{table}'")
        node.succeed(f"dmsetup resume {mapper}")


def dm_delay_deactivate(node, names):
    dm_delay_activate(node, names)


def dm_delay_remove(node, names):
    for name in _dm_delay_names(names):
        node.execute(f"rm -f /dev/disk/by-id/braid-test-{name}-delay")
        node.execute(f"dmsetup remove {name}-delay 2>/dev/null || true")
