namespace Vorcall.Server.Data;

// A banned account. The row is the gate: login, refresh and the WebSocket upgrade all refuse
// while it exists. The account itself stays, with every message it sent tombstoned.
public class Ban
{
    public long UserId { get; set; }

    // Null when the account that issued the ban has since been deleted.
    public long? BannedBy { get; set; }

    public string Reason { get; set; } = string.Empty;

    public DateTime BannedAt { get; set; }
}
