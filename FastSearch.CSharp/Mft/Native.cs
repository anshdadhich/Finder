using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

namespace FastSearch.Mft;

internal static class Native
{
    internal const uint GENERIC_READ = 0x80000000;
    internal const uint FILE_SHARE_READ = 0x00000001;
    internal const uint FILE_SHARE_WRITE = 0x00000002;
    internal const uint FILE_SHARE_DELETE = 0x00000004;
    internal const uint OPEN_EXISTING = 3;
    internal const uint FILE_FLAG_BACKUP_SEMANTICS = 0x02000000;
    internal const uint FILE_FLAG_SEQUENTIAL_SCAN = 0x08000000;
    internal const uint FILE_BEGIN = 0;
    internal const uint FILE_ATTRIBUTE_DIRECTORY = 0x10;

    internal const uint FSCTL_ENUM_USN_DATA = 0x000900b3;
    internal const uint FSCTL_QUERY_USN_JOURNAL = 0x000900f4;
    internal const uint FSCTL_READ_USN_JOURNAL = 0x000900bb;

    internal const uint USN_REASON_FILE_CREATE = 0x00000100;
    internal const uint USN_REASON_FILE_DELETE = 0x00000200;
    internal const uint USN_REASON_RENAME_OLD_NAME = 0x00001000;
    internal const uint USN_REASON_RENAME_NEW_NAME = 0x00002000;

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    internal static extern SafeFileHandle CreateFileW(
        string lpFileName,
        uint dwDesiredAccess,
        uint dwShareMode,
        IntPtr lpSecurityAttributes,
        uint dwCreationDisposition,
        uint dwFlagsAndAttributes,
        IntPtr hTemplateFile);

    [DllImport("kernel32.dll", SetLastError = true)]
    internal static extern bool ReadFile(
        SafeFileHandle hFile,
        byte[] lpBuffer,
        uint nNumberOfBytesToRead,
        out uint lpNumberOfBytesRead,
        IntPtr lpOverlapped);

    [DllImport("kernel32.dll", SetLastError = true)]
    internal static extern bool ReadFile(
        SafeFileHandle hFile,
        IntPtr lpBuffer,
        uint nNumberOfBytesToRead,
        out uint lpNumberOfBytesRead,
        IntPtr lpOverlapped);

    [DllImport("kernel32.dll", SetLastError = true)]
    internal static extern bool SetFilePointerEx(
        SafeFileHandle hFile,
        long liDistanceToMove,
        IntPtr lpNewFilePointer,
        uint dwMoveMethod);

    [DllImport("kernel32.dll", SetLastError = true)]
    internal static extern bool DeviceIoControl(
        SafeFileHandle hDevice,
        uint dwIoControlCode,
        IntPtr lpInBuffer,
        uint nInBufferSize,
        IntPtr lpOutBuffer,
        uint nOutBufferSize,
        out uint lpBytesReturned,
        IntPtr lpOverlapped);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    internal static extern uint GetLogicalDriveStringsW(uint nBufferLength, char[] lpBuffer);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    internal static extern bool GetVolumeInformationW(
        string lpRootPathName,
        IntPtr lpVolumeNameBuffer,
        uint nVolumeNameSize,
        IntPtr lpVolumeSerialNumber,
        IntPtr lpMaximumComponentLength,
        IntPtr lpFileSystemFlags,
        char[] lpFileSystemNameBuffer,
        uint nFileSystemNameSize);

    [StructLayout(LayoutKind.Sequential)]
    internal struct MftEnumDataV0
    {
        public ulong StartFileReferenceNumber;
        public long LowUsn;
        public long HighUsn;
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct UsnJournalDataV0
    {
        public ulong UsnJournalID;
        public long FirstUsn;
        public long NextUsn;
        public long LowestValidUsn;
        public long MaxUsn;
        public ulong MaximumSize;
        public ulong AllocationDelta;
    }

    [StructLayout(LayoutKind.Sequential)]
    internal struct ReadUsnJournalDataV0
    {
        public long StartUsn;
        public uint ReasonMask;
        public uint ReturnOnlyOnClose;
        public ulong Timeout;
        public ulong BytesToWaitFor;
        public ulong UsnJournalID;
    }
}
