using System.Text.Json.Serialization;
using System.Runtime.InteropServices;
using FastSearch.Mft;

namespace FastSearch.Index;

public sealed record CachedEntry(ulong FileRef, ulong ParentRef, string Name, FileKind Kind);

public sealed class CacheData
{
    public List<CachedEntry> Entries { get; set; } = [];
    public string DriveRoot { get; set; } = "";
    public List<JournalCheckpoint> Checkpoints { get; set; } = [];
}

public sealed class IndexEntry
{
    public ulong FileRef;
    public ulong ParentRef;
    public uint NameOff;
    public uint NameLowerOff;
    public ushort NameLen;
    public ushort NameLowerLen;
    public byte Flags;

    [JsonIgnore]
    public bool IsDir => (Flags & 1) != 0;

    public FileKind Kind() => IsDir ? FileKind.Directory : FileKind.File;
}

public sealed class IndexStore
{
    public List<IndexEntry> Entries { get; } = [];
    public List<byte> NameArena { get; } = [];
    public List<byte> NameLowerArena { get; } = [];
    public List<(ulong Ref, uint Idx)> RefLookup { get; } = [];
    public string DriveRoot { get; set; } = "";
    public List<JournalCheckpoint> Checkpoints { get; set; } = [];

    public string Name(IndexEntry e) => System.Text.Encoding.UTF8.GetString(CollectionsMarshal.AsSpan(NameArena).Slice((int)e.NameOff, e.NameLen));
    public string NameLower(IndexEntry e) => System.Text.Encoding.UTF8.GetString(CollectionsMarshal.AsSpan(NameLowerArena).Slice((int)e.NameLowerOff, e.NameLowerLen));

    public uint? LookupIdx(ulong fileRef)
    {
        var lo = 0;
        var hi = RefLookup.Count - 1;
        while (lo <= hi)
        {
            var mid = lo + ((hi - lo) / 2);
            var value = RefLookup[mid].Ref;
            if (value == fileRef) return RefLookup[mid].Idx;
            if (value < fileRef) lo = mid + 1; else hi = mid - 1;
        }
        return null;
    }

    private void RebuildRefLookup()
    {
        RefLookup.Clear();
        for (var i = 0; i < Entries.Count; i++) RefLookup.Add((Entries[i].FileRef, (uint)i));
        RefLookup.Sort((a, b) => a.Ref.CompareTo(b.Ref));
    }

    public void PopulateFromScan(ScanResult scan, string driveRoot)
    {
        DriveRoot = driveRoot;
        Entries.Capacity = Math.Max(Entries.Capacity, Entries.Count + scan.Records.Count);
        NameArena.Capacity = Math.Max(NameArena.Capacity, NameArena.Count + scan.Records.Count * 30);
        NameLowerArena.Capacity = Math.Max(NameLowerArena.Capacity, NameLowerArena.Count + scan.Records.Count * 30);

        foreach (var r in scan.Records)
        {
            var name = new string(CollectionsMarshal.AsSpan(scan.NameData).Slice((int)r.NameOff, r.NameLen));
            AddEntry(r.FileRef, r.ParentRef, name, r.IsDir ? FileKind.Directory : FileKind.File, sorted: false);
        }
    }

    public void FinalizeIndex()
    {
        Entries.Sort((a, b) => string.CompareOrdinal(NameLower(a), NameLower(b)));
        RebuildRefLookup();
        NameArena.TrimExcess();
        NameLowerArena.TrimExcess();
    }

    public CacheData ToCache() => new()
    {
        Entries = Entries.Select(e => new CachedEntry(e.FileRef, e.ParentRef, Name(e), e.Kind())).ToList(),
        DriveRoot = DriveRoot,
        Checkpoints = Checkpoints.ToList(),
    };

    public static IndexStore FromCache(CacheData cache)
    {
        var store = new IndexStore { DriveRoot = cache.DriveRoot, Checkpoints = cache.Checkpoints.ToList() };
        store.Entries.Capacity = cache.Entries.Count;
        store.NameArena.Capacity = cache.Entries.Count * 30;
        store.NameLowerArena.Capacity = cache.Entries.Count * 30;

        foreach (var c in cache.Entries) store.AddEntry(c.FileRef, c.ParentRef, c.Name, c.Kind, sorted: false);
        store.RebuildRefLookup();
        store.NameArena.TrimExcess();
        store.NameLowerArena.TrimExcess();
        return store;
    }

    public void Insert(FileRecord record) => AddEntry(record.FileRef, record.ParentRef, record.Name, record.Kind, sorted: true);

    public void Remove(ulong fileRef)
    {
        Entries.RemoveAll(e => e.FileRef == fileRef);
        RebuildRefLookup();
    }

    public void Rename(ulong oldRef, FileRecord newRecord)
    {
        Remove(oldRef);
        Insert(newRecord);
    }

    public void ApplyMove(ulong fileRef, ulong newParentRef, string name, FileKind kind)
    {
        Remove(fileRef);
        Insert(new FileRecord(fileRef, newParentRef, name, kind));
    }

    public int Len() => Entries.Count;

    private void AddEntry(ulong fileRef, ulong parentRef, string name, FileKind kind, bool sorted)
    {
        var nameLower = name.ToLowerInvariant();
        var nameBytes = System.Text.Encoding.UTF8.GetBytes(name);
        var nameLowerBytes = System.Text.Encoding.UTF8.GetBytes(nameLower);

        var nOff = (uint)NameArena.Count;
        NameArena.AddRange(nameBytes);
        var nlOff = (uint)NameLowerArena.Count;
        NameLowerArena.AddRange(nameLowerBytes);

        var entry = new IndexEntry
        {
            FileRef = fileRef,
            ParentRef = parentRef,
            NameOff = nOff,
            NameLowerOff = nlOff,
            NameLen = (ushort)nameBytes.Length,
            NameLowerLen = (ushort)nameLowerBytes.Length,
            Flags = kind == FileKind.Directory ? (byte)1 : (byte)0,
        };

        if (!sorted)
        {
            Entries.Add(entry);
            return;
        }

        var pos = Entries.FindIndex(e => string.CompareOrdinal(NameLower(e), nameLower) >= 0);
        if (pos < 0) Entries.Add(entry); else Entries.Insert(pos, entry);
        RebuildRefLookup();
    }
}
