namespace Vorcall.Server.Data;

// Stored as a smallint whose values are the ones Protocol.RoomKind uses on the wire, so the two
// are one cast apart; 0 (unspecified) is never persisted.
public enum RoomKind
{
    Public = 1,
    Dm = 2,
}

public class Room
{
    // The room id is the key: a public room's slug, or dm-<lower user id>-<higher user id>.
    public string Id { get; set; } = string.Empty;

    public RoomKind Kind { get; set; }

    // Display name, in the casing that was typed; empty for DMs, where the client shows the
    // other member's username instead.
    public string Name { get; set; } = string.Empty;

    // Null for general, which no account created, and for a room whose creator was deleted.
    public long? CreatedBy { get; set; }

    public DateTime CreatedAt { get; set; }
}
