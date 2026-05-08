using System.Buffers.Binary;
using System.Runtime.InteropServices;
using Microsoft.Win32.SafeHandles;

namespace FastSearch.Mft;

public sealed record CompactRecord(ulong FileRef, ulong ParentRef, uint NameOff, ushort NameLen, bool IsDir);

public sealed class ScanResult
{
    public List<CompactRecord> Records { get; } = new(3_000_000);
    public List<char> NameData { get; } = new(40_000_000);
}

public sealed class MftReader : IDisposable
{
    private const int FallbackBuf = 4 * 1024 * 1024;
    private const int DirectBuf = 4 * 1024 * 1024;

    private readonly SafeFileHandle _handle;
    public NtfsDrive Drive { get; }

    private MftReader(SafeFileHandle handle, NtfsDrive drive)
    {
        _handle = handle;
        Drive = drive;
    }

    public static MftReader Open(NtfsDrive drive)
    {
        var handle = Native.CreateFileW(
            drive.DevicePath,
            Native.GENERIC_READ,
            Native.FILE_SHARE_READ | Native.FILE_SHARE_WRITE | Native.FILE_SHARE_DELETE,
            IntPtr.Zero,
            Native.OPEN_EXISTING,
            Native.FILE_FLAG_BACKUP_SEMANTICS,
            IntPtr.Zero);

        if (handle.IsInvalid) throw new IOException($"CreateFileW failed: {Marshal.GetLastWin32Error()}");
        return new MftReader(handle, drive);
    }

    public ScanResult? ScanDirect()
    {
        var recordSize = ReadMftRecordSize();
        if (recordSize is null) return null;

        var mftHandle = Native.CreateFileW(
            $"{Drive.Root}$MFT",
            Native.GENERIC_READ,
            Native.FILE_SHARE_READ | Native.FILE_SHARE_WRITE | Native.FILE_SHARE_DELETE,
            IntPtr.Zero,
            Native.OPEN_EXISTING,
            Native.FILE_FLAG_BACKUP_SEMANTICS | Native.FILE_FLAG_SEQUENTIAL_SCAN,
            IntPtr.Zero);

        if (mftHandle.IsInvalid) return null;

        using (mftHandle)
        {
            var scan = new ScanResult();
            var buffer = new byte[DirectBuf];
            ulong mftIndex = 0;
            var leftover = 0;

            while (true)
            {
                var readSize = (uint)(buffer.Length - leftover);
                unsafe
                {
                    fixed (byte* ptr = buffer)
                    {
                        var ok = Native.ReadFile(mftHandle, (IntPtr)(ptr + leftover), readSize, out var bytesRead, IntPtr.Zero);
                        if (!ok || bytesRead == 0) break;

                        var total = leftover + (int)bytesRead;
                        var offset = 0;
                        while (offset + recordSize.Value <= total)
                        {
                            var slice = buffer.AsSpan(offset, recordSize.Value);
                            if (ApplyFixup(slice, recordSize.Value))
                            {
                                ParseFileRecord(slice, mftIndex, scan);
                            }

                            mftIndex++;
                            offset += recordSize.Value;
                        }

                        offset = total - total % recordSize.Value;
                        leftover = total - offset;
                        if (leftover > 0) Buffer.BlockCopy(buffer, offset, buffer, 0, leftover);
                    }
                }
            }

            return scan;
        }
    }

    public ScanResult Scan()
    {
        var scan = new ScanResult();
        var enumData = new Native.MftEnumDataV0 { StartFileReferenceNumber = 0, LowUsn = 0, HighUsn = long.MaxValue };
        var buffer = new byte[FallbackBuf];
        var inSize = Marshal.SizeOf<Native.MftEnumDataV0>();

        unsafe
        {
            fixed (byte* outPtr = buffer)
            {
                while (true)
                {
                    var inPtr = Marshal.AllocHGlobal(inSize);
                    try
                    {
                        Marshal.StructureToPtr(enumData, inPtr, false);
                        var ok = Native.DeviceIoControl(_handle, Native.FSCTL_ENUM_USN_DATA, inPtr, (uint)inSize, (IntPtr)outPtr, FallbackBuf, out var bytesReturned, IntPtr.Zero);
                        if (!ok)
                        {
                            var code = Marshal.GetLastWin32Error();
                            if (code != 38) Console.Error.WriteLine($"MFT error on {Drive.Letter}: {code}");
                            break;
                        }

                        if (bytesReturned <= 8) break;
                        enumData.StartFileReferenceNumber = BinaryPrimitives.ReadUInt64LittleEndian(buffer.AsSpan(0, 8));

                        var offset = 8;
                        while (offset + 60 <= bytesReturned)
                        {
                            var recLen = BinaryPrimitives.ReadUInt32LittleEndian(buffer.AsSpan(offset, 4));
                            if (recLen == 0 || offset + recLen > bytesReturned) break;

                            var fileRef = BinaryPrimitives.ReadUInt64LittleEndian(buffer.AsSpan(offset + 8, 8));
                            var parentRef = BinaryPrimitives.ReadUInt64LittleEndian(buffer.AsSpan(offset + 16, 8));
                            var attrs = BinaryPrimitives.ReadUInt32LittleEndian(buffer.AsSpan(offset + 52, 4));
                            var nameLen = BinaryPrimitives.ReadUInt16LittleEndian(buffer.AsSpan(offset + 56, 2)) / 2;
                            var nameOffset = BinaryPrimitives.ReadUInt16LittleEndian(buffer.AsSpan(offset + 58, 2));
                            var arenaOff = (uint)scan.NameData.Count;

                            for (var i = 0; i < nameLen; i++)
                            {
                                scan.NameData.Add((char)BinaryPrimitives.ReadUInt16LittleEndian(buffer.AsSpan(offset + nameOffset + i * 2, 2)));
                            }

                            scan.Records.Add(new CompactRecord(fileRef, parentRef, arenaOff, (ushort)nameLen, (attrs & Native.FILE_ATTRIBUTE_DIRECTORY) != 0));
                            offset += (int)recLen;
                        }
                    }
                    finally
                    {
                        Marshal.FreeHGlobal(inPtr);
                    }
                }
            }
        }

        return scan;
    }

    private int? ReadMftRecordSize()
    {
        if (!Native.SetFilePointerEx(_handle, 0, IntPtr.Zero, Native.FILE_BEGIN)) return null;
        var boot = new byte[512];
        if (!Native.ReadFile(_handle, boot, 512, out var br, IntPtr.Zero) || br < 512) return null;
        if (boot[3] != (byte)'N' || boot[4] != (byte)'T' || boot[5] != (byte)'F' || boot[6] != (byte)'S') return null;

        var bytesPerSector = BinaryPrimitives.ReadUInt16LittleEndian(boot.AsSpan(0x0B, 2));
        var sectorsPerCluster = boot[0x0D];
        var clusterSize = bytesPerSector * sectorsPerCluster;
        var raw = unchecked((sbyte)boot[0x40]);
        return raw > 0 ? raw * clusterSize : 1 << -raw;
    }

    private static bool ApplyFixup(Span<byte> record, int recordSize)
    {
        if (record.Length < 48 || record[0] != (byte)'F' || record[1] != (byte)'I' || record[2] != (byte)'L' || record[3] != (byte)'E') return false;

        var fixupOff = BinaryPrimitives.ReadUInt16LittleEndian(record.Slice(4, 2));
        var fixupCnt = BinaryPrimitives.ReadUInt16LittleEndian(record.Slice(6, 2));
        if (fixupCnt < 2 || fixupOff + fixupCnt * 2 > recordSize) return false;

        var check0 = record[fixupOff];
        var check1 = record[fixupOff + 1];

        for (var i = 1; i < fixupCnt; i++)
        {
            var end = i * 512 - 2;
            if (end + 1 >= recordSize) break;
            if (record[end] != check0 || record[end + 1] != check1) return false;
            record[end] = record[fixupOff + i * 2];
            record[end + 1] = record[fixupOff + i * 2 + 1];
        }

        return true;
    }

    private static void ParseFileRecord(ReadOnlySpan<byte> record, ulong mftIndex, ScanResult scan)
    {
        var flags = BinaryPrimitives.ReadUInt16LittleEndian(record.Slice(0x16, 2));
        if ((flags & 0x01) == 0) return;

        var isDir = (flags & 0x02) != 0;
        var seq = BinaryPrimitives.ReadUInt16LittleEndian(record.Slice(0x10, 2));
        var fileRef = mftIndex | ((ulong)seq << 48);
        var aoff = (int)BinaryPrimitives.ReadUInt16LittleEndian(record.Slice(0x14, 2));
        byte bestNs = 255;
        (int pos, int len, ulong parent)? bestName = null;

        while (aoff + 8 <= record.Length)
        {
            var atype = BinaryPrimitives.ReadUInt32LittleEndian(record.Slice(aoff, 4));
            if (atype == 0xffff_ffff) break;
            var alen = (int)BinaryPrimitives.ReadUInt32LittleEndian(record.Slice(aoff + 4, 4));
            if (alen == 0 || aoff + alen > record.Length) break;

            if (atype == 0x30 && record[aoff + 8] == 0)
            {
                var vlen = (int)BinaryPrimitives.ReadUInt32LittleEndian(record.Slice(aoff + 16, 4));
                var voff = BinaryPrimitives.ReadUInt16LittleEndian(record.Slice(aoff + 20, 2));
                var vs = aoff + voff;
                if (vs + 66 <= record.Length && vlen >= 66)
                {
                    var parent = BinaryPrimitives.ReadUInt64LittleEndian(record.Slice(vs, 8));
                    var nlen = record[vs + 64];
                    var ns = record[vs + 65];
                    if (vs + 66 + nlen * 2 <= record.Length && ns != 2)
                    {
                        byte priority = ns switch { 1 => 0, 3 => 1, 0 => 2, _ => 3 };
                        if (priority < bestNs)
                        {
                            bestNs = priority;
                            bestName = (vs + 66, nlen, parent);
                            if (priority == 0) break;
                        }
                    }
                }
            }

            aoff += alen;
        }

        if (bestName is not { } best) return;
        var arenaOff = (uint)scan.NameData.Count;
        for (var i = 0; i < best.len; i++)
        {
            scan.NameData.Add((char)BinaryPrimitives.ReadUInt16LittleEndian(record.Slice(best.pos + i * 2, 2)));
        }

        scan.Records.Add(new CompactRecord(fileRef, best.parent, arenaOff, (ushort)best.len, isDir));
    }

    public void Dispose() => _handle.Dispose();
}
