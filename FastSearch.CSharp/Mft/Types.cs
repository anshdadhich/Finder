namespace FastSearch.Mft;

public enum FileKind
{
    File,
    Directory,
}

public sealed record FileRecord(ulong FileRef, ulong ParentRef, string Name, FileKind Kind);

public sealed record NtfsDrive(char Letter, string Root, string DevicePath);

public abstract record IndexEvent
{
    public sealed record Created(FileRecord Record) : IndexEvent;
    public sealed record Deleted(ulong FileRef) : IndexEvent;
    public sealed record Renamed(ulong OldRef, FileRecord NewRecord) : IndexEvent;
    public sealed record Moved(ulong FileRef, ulong NewParentRef, string Name, FileKind Kind) : IndexEvent;
}

public sealed record JournalCheckpoint(long NextUsn, ulong JournalId, char DriveLetter);
