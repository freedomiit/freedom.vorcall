namespace Vorcall.Server.Data;

// Stored as a smallint whose values are the ones Protocol.ChannelKind uses on the wire, so the
// two are one cast apart; 0 (unspecified) is never persisted.
public enum ChannelKind
{
    Text = 1,
    Voice = 2,
    Dm = 3,
}

public class Channel
{
    public long Id { get; set; }

    public ChannelKind Kind { get; set; }

    // Display name, in the casing that was typed, and not unique; empty for a DM, where the
    // client shows the other member's name instead.
    public string Name { get; set; } = string.Empty;

    public string Topic { get; set; } = string.Empty;

    // Null for a DM and for a channel that sits above every category.
    public long? CategoryId { get; set; }

    public int Position { get; set; }

    public DateTime CreatedAt { get; set; }

    // The two members of a DM, lower id first; both null for a text or voice channel. The pair is
    // unique, which is what keeps a second DM between the same two accounts from existing.
    public long? DmLow { get; set; }

    public long? DmHigh { get; set; }
}
