using System.IO;
using System.IO.Compression;
using System.Text;
using FastSearch.Index;
using FastSearch.Mft;

namespace FastSearch;

public static class CacheStore
{
    private const uint Magic = 0x46534348;
    private const int Version = 1;

    public static CacheData Load(string path)
    {
        using var file = File.OpenRead(path);
        using var br = new BinaryReader(file, Encoding.UTF8, leaveOpen: false);
        if (br.ReadUInt32() != Magic) throw new InvalidDataException("Bad cache magic");
        if (br.ReadInt32() != Version) throw new InvalidDataException("Bad cache version");

        using var gzip = new GZipStream(file, System.IO.Compression.CompressionMode.Decompress, leaveOpen: false);
        using var data = new BinaryReader(gzip, Encoding.UTF8);

        var cache = new CacheData
        {
            DriveRoot = data.ReadString(),
        };

        var checkpointCount = data.ReadInt32();
        for (var i = 0; i < checkpointCount; i++)
        {
            cache.Checkpoints.Add(new JournalCheckpoint(
                data.ReadInt64(),
                data.ReadUInt64(),
                data.ReadChar()));
        }

        var entryCount = data.ReadInt32();
        cache.Entries.Capacity = entryCount;
        for (var i = 0; i < entryCount; i++)
        {
            cache.Entries.Add(new CachedEntry(
                data.ReadUInt64(),
                data.ReadUInt64(),
                data.ReadString(),
                data.ReadByte() == 1 ? FileKind.Directory : FileKind.File));
        }

        return cache;
    }

    public static void SaveAtomic(string path, CacheData cache)
    {
        var dir = Path.GetDirectoryName(path)!;
        Directory.CreateDirectory(dir);
        var temp = Path.Combine(dir, $"{Path.GetFileName(path)}.{Environment.ProcessId}.tmp");

        try
        {
            using (var file = File.Create(temp))
            {
                using var bw = new BinaryWriter(file, Encoding.UTF8, leaveOpen: true);
                bw.Write(Magic);
                bw.Write(Version);
                bw.Flush();

                using var gzip = new GZipStream(file, CompressionLevel.Fastest, leaveOpen: false);
                using var data = new BinaryWriter(gzip, Encoding.UTF8);
                data.Write(cache.DriveRoot);
                data.Write(cache.Checkpoints.Count);
                foreach (var cp in cache.Checkpoints)
                {
                    data.Write(cp.NextUsn);
                    data.Write(cp.JournalId);
                    data.Write(cp.DriveLetter);
                }

                data.Write(cache.Entries.Count);
                foreach (var e in cache.Entries)
                {
                    data.Write(e.FileRef);
                    data.Write(e.ParentRef);
                    data.Write(e.Name);
                    data.Write(e.Kind == FileKind.Directory ? (byte)1 : (byte)0);
                }
            }

            if (File.Exists(path)) File.Replace(temp, path, null);
            else File.Move(temp, path);
        }
        finally
        {
            try { if (File.Exists(temp)) File.Delete(temp); } catch { }
        }
    }
}
