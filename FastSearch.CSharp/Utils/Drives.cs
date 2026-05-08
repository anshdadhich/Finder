using FastSearch.Mft;

namespace FastSearch.Utils;

public static class Drives
{
    public static List<NtfsDrive> GetNtfsDrives()
    {
        var drives = new List<NtfsDrive>();
        var buffer = new char[256];
        var len = Native.GetLogicalDriveStringsW((uint)buffer.Length, buffer);
        if (len == 0) return drives;

        var all = new string(buffer, 0, (int)len);
        foreach (var root in all.Split('\0', StringSplitOptions.RemoveEmptyEntries))
        {
            if (!IsNtfs(root)) continue;
            var letter = root[0];
            drives.Add(new NtfsDrive(letter, root, $@"\\.\{letter}:"));
        }

        return drives;
    }

    private static bool IsNtfs(string root)
    {
        var fsName = new char[32];
        if (!Native.GetVolumeInformationW(root, IntPtr.Zero, 0, IntPtr.Zero, IntPtr.Zero, IntPtr.Zero, fsName, (uint)fsName.Length))
        {
            return false;
        }

        return new string(fsName).TrimEnd('\0').StartsWith("NTFS", StringComparison.Ordinal);
    }
}
