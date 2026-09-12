namespace Vorcall.Server.Data;

// The one server row, keyed by a constant id so every read is a single-row lookup and no code
// has to carry a server id around. There is exactly one server.
public class Server
{
    public const short RowId = 1;

    public short Id { get; set; } = RowId;

    public string Name { get; set; } = "Vorcall";

    public string Description { get; set; } = string.Empty;

    public long? IconImageId { get; set; }

    // Null only until an account exists to own the server; the owner is the one bypass of every
    // permission check.
    public long? OwnerId { get; set; }

    // The text channel nobody can be denied sight of and nobody can delete.
    public long? GeneralChannelId { get; set; }
}
