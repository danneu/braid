use std::fs::File;
use std::os::unix::io::AsRawFd;

/// ioctl number for `HDIO_DRIVE_CMD` (from `<linux/hdreg.h>`).
const HDIO_DRIVE_CMD: u64 = 0x031f;

/// ATA CHECK POWER MODE opcode (ATA-8 / ACS).
const ATA_OP_CHECKPOWERMODE: u8 = 0xE5;

/// Power state of a SATA drive, as reported by ATA CHECK POWER MODE.
///
/// Querying this does **not** wake a sleeping/standby drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrivePowerState {
    /// Platters stopped (drive spun down).
    Standby,
    /// Heads parked but platters still spinning.
    Idle,
    /// Fully operational.
    Active,
    /// Drive returned an unrecognised power-state byte.
    Unknown(u8),
}

/// Query the current power state of a SATA drive without waking it.
///
/// Uses the `HDIO_DRIVE_CMD` ioctl with ATA CHECK POWER MODE — the same
/// mechanism as `hdparm -C`.
///
/// `path` should be a block device path like `/dev/sda`.
pub fn check_power_mode(path: &str) -> std::io::Result<DrivePowerState> {
    let file = File::open(path)?;
    let fd = file.as_raw_fd();
    let mut args: [u8; 4] = [ATA_OP_CHECKPOWERMODE, 0, 0, 0];

    // SAFETY: HDIO_DRIVE_CMD reads/writes a 4-byte buffer and is a
    // standard Linux ATA ioctl.  We own `args` and `fd` is a valid fd.
    let ret = unsafe { libc::ioctl(fd, HDIO_DRIVE_CMD, args.as_mut_ptr()) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(match args[2] {
        0x00 => DrivePowerState::Standby,
        0x80 => DrivePowerState::Idle,
        0xFF => DrivePowerState::Active,
        other => DrivePowerState::Unknown(other),
    })
}
