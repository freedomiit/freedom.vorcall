namespace Vorcall.Server.Data;

public class Message
{
    public long Id { get; set; }

    public string Author { get; set; } = string.Empty;

    public string Text { get; set; } = string.Empty;

    public DateTime SentAt { get; set; }
}
