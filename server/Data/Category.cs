namespace Vorcall.Server.Data;

// A named group of channels. Categories order themselves by Position; the channels inside one
// order themselves by their own.
public class Category
{
    public long Id { get; set; }

    public string Name { get; set; } = string.Empty;

    public int Position { get; set; }
}
