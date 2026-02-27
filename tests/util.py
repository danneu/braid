# Shared helpers for repro test scripts


class util:
    @staticmethod
    def write_file_mib(path: str, count_mib: int) -> None:
        machine.succeed(f"dd if=/dev/urandom of={path} bs=1M count={count_mib}")
        machine.succeed("sync")
