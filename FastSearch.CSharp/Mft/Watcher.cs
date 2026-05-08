using System.Buffers.Binary;
using System.Runtime.InteropServices;
using System.Threading.Channels;
using Microsoft.Win32.SafeHandles;

namespace FastSearch.Mft;

public sealed class UsnWatcher : IDisposable
{
    private const int BufferSize = 64 * 1024;

    private readonly SafeFileHandle _handle;
    private readonly NtfsDrive _drive;
    private readonly ChannelWriter<IndexEvent> _sender;

    public long NextUsn { get; private set; }
    public ulong JournalId { get; }

    private UsnWatcher(SafeFileHandle handle, NtfsDrive drive, ChannelWriter<IndexEvent> sender, long nextUsn, ulong journalId)
    {
        _handle = handle;
        _drive = drive;
        _sender = sender;
        NextUsn = nextUsn;
        JournalId = journalId;
    }

    public static UsnWatcher New(NtfsDrive drive, ChannelWriter<IndexEvent> sender) => NewFrom(drive, sender, null);

    public static UsnWatcher NewFrom(NtfsDrive drive, ChannelWriter<IndexEvent> sender, JournalCheckpoint? checkpoint)
    {
        var handle = Native.CreateFileW(
            drive.DevicePath,
            0,
            Native.FILE_SHARE_READ | Native.FILE_SHARE_WRITE | Native.FILE_SHARE_DELETE,
            IntPtr.Zero,
            Native.OPEN_EXISTING,
            Native.FILE_FLAG_BACKUP_SEMANTICS,
            IntPtr.Zero);

        if (handle.IsInvalid) throw new IOException($"CreateFileW failed: {Marshal.GetLastWin32Error()}");

        var journalData = QueryJournal(handle);
        long nextUsn;
        if (checkpoint is not null)
        {
            if (checkpoint.JournalId != journalData.UsnJournalID) throw new IOException("Journal ID mismatch - rescan needed");
            if (checkpoint.NextUsn < journalData.FirstUsn || checkpoint.NextUsn > journalData.NextUsn) throw new IOException("Saved USN outside journal range - rescan needed");
            nextUsn = checkpoint.NextUsn;
        }
        else
        {
            nextUsn = journalData.NextUsn;
        }

        return new UsnWatcher(handle, drive, sender, nextUsn, journalData.UsnJournalID);
    }

    public JournalCheckpoint Checkpoint() => new(NextUsn, JournalId, _drive.Letter);

    public void Run()
    {
        var buffer = new byte[BufferSize];
        while (true)
        {
            Thread.Sleep(500);
            Poll(buffer);
        }
    }

    public void RunShared(List<JournalCheckpoint> shared, object gate)
    {
        var buffer = new byte[BufferSize];
        while (true)
        {
            Thread.Sleep(500);
            Poll(buffer);
            lock (gate)
            {
                shared.RemoveAll(c => c.DriveLetter == _drive.Letter);
                shared.Add(Checkpoint());
            }
        }
    }

    public int Drain()
    {
        var buffer = new byte[BufferSize];
        var count = 0;
        while (true)
        {
            var before = NextUsn;
            Poll(buffer);
            if (NextUsn == before) break;
            count++;
        }
        return count;
    }

    private void Poll(byte[] buffer)
    {
        var readData = new Native.ReadUsnJournalDataV0
        {
            StartUsn = NextUsn,
            ReasonMask = Native.USN_REASON_FILE_CREATE | Native.USN_REASON_FILE_DELETE | Native.USN_REASON_RENAME_NEW_NAME | Native.USN_REASON_RENAME_OLD_NAME,
            ReturnOnlyOnClose = 0,
            Timeout = 0,
            BytesToWaitFor = 0,
            UsnJournalID = JournalId,
        };

        var inSize = Marshal.SizeOf<Native.ReadUsnJournalDataV0>();
        var inPtr = Marshal.AllocHGlobal(inSize);
        try
        {
            Marshal.StructureToPtr(readData, inPtr, false);
            unsafe
            {
                fixed (byte* outPtr = buffer)
                {
                    var ok = Native.DeviceIoControl(_handle, Native.FSCTL_READ_USN_JOURNAL, inPtr, (uint)inSize, (IntPtr)outPtr, BufferSize, out var bytesReturned, IntPtr.Zero);
                    if (!ok || bytesReturned <= 8) return;
                    NextUsn = BinaryPrimitives.ReadInt64LittleEndian(buffer.AsSpan(0, 8));

                    var offset = 8;
                    while (offset + 60 <= bytesReturned)
                    {
                        var recLen = BinaryPrimitives.ReadUInt32LittleEndian(buffer.AsSpan(offset, 4));
                        if (recLen == 0) break;
                        ProcessRecord(buffer, offset);
                        offset += (int)recLen;
                    }
                }
            }
        }
        finally
        {
            Marshal.FreeHGlobal(inPtr);
        }
    }

    private void ProcessRecord(byte[] buffer, int offset)
    {
        var fileRef = BinaryPrimitives.ReadUInt64LittleEndian(buffer.AsSpan(offset + 8, 8));
        var parentRef = BinaryPrimitives.ReadUInt64LittleEndian(buffer.AsSpan(offset + 16, 8));
        var reason = BinaryPrimitives.ReadUInt32LittleEndian(buffer.AsSpan(offset + 40, 4));
        var attrs = BinaryPrimitives.ReadUInt32LittleEndian(buffer.AsSpan(offset + 52, 4));
        var nameLen = BinaryPrimitives.ReadUInt16LittleEndian(buffer.AsSpan(offset + 56, 2)) / 2;
        var nameOffset = BinaryPrimitives.ReadUInt16LittleEndian(buffer.AsSpan(offset + 58, 2));
        var name = new string(MemoryMarshal.Cast<byte, char>(buffer.AsSpan(offset + nameOffset, nameLen * 2)));

        if ((reason & Native.USN_REASON_FILE_DELETE) != 0)
        {
            _sender.TryWrite(new IndexEvent.Deleted(fileRef));
            return;
        }

        var kind = (attrs & Native.FILE_ATTRIBUTE_DIRECTORY) != 0 ? FileKind.Directory : FileKind.File;
        if ((reason & Native.USN_REASON_RENAME_NEW_NAME) != 0)
        {
            _sender.TryWrite(new IndexEvent.Moved(fileRef, parentRef, name, kind));
            return;
        }

        if ((reason & Native.USN_REASON_FILE_CREATE) != 0)
        {
            _sender.TryWrite(new IndexEvent.Created(new FileRecord(fileRef, parentRef, name, kind)));
        }
    }

    private static Native.UsnJournalDataV0 QueryJournal(SafeFileHandle handle)
    {
        var size = Marshal.SizeOf<Native.UsnJournalDataV0>();
        var outPtr = Marshal.AllocHGlobal(size);
        try
        {
            var ok = Native.DeviceIoControl(handle, Native.FSCTL_QUERY_USN_JOURNAL, IntPtr.Zero, 0, outPtr, (uint)size, out _, IntPtr.Zero);
            if (!ok) throw new IOException($"FSCTL_QUERY_USN_JOURNAL failed: {Marshal.GetLastWin32Error()}");
            return Marshal.PtrToStructure<Native.UsnJournalDataV0>(outPtr);
        }
        finally
        {
            Marshal.FreeHGlobal(outPtr);
        }
    }

    public void Dispose() => _handle.Dispose();
}
