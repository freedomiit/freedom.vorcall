using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Data;
using Vorcall.Server.Protocol;

namespace Vorcall.Server.Chat;

public sealed class MessageService(IDbContextFactory<AppDbContext> contextFactory)
{
    public async Task<ChatMessage> AppendAsync(string author, string text)
    {
        await using var db = await contextFactory.CreateDbContextAsync();
        var message = new Data.Message
        {
            Author = author,
            Text = text,
            SentAt = DateTime.UtcNow,
        };

        db.Messages.Add(message);
        await db.SaveChangesAsync();
        return ToProtocol(message);
    }

    public async Task<long> GetLatestIdAsync()
    {
        await using var db = await contextFactory.CreateDbContextAsync();
        return await db.Messages.MaxAsync(m => (long?)m.Id) ?? 0;
    }

    public async Task<MessagePage> GetPageAsync(int limit, long? before)
    {
        await using var db = await contextFactory.CreateDbContextAsync();
        var query = db.Messages.AsNoTracking();
        if (before is { } exclusiveUpperBound)
        {
            query = query.Where(m => m.Id < exclusiveUpperBound);
        }

        // One row past the page tells us whether older messages exist, without a second query.
        var rows = await query
            .OrderByDescending(m => m.Id)
            .Take(limit + 1)
            .ToListAsync();

        var page = new MessagePage { HasMore = rows.Count > limit };
        if (rows.Count > limit)
        {
            rows.RemoveRange(limit, rows.Count - limit);
        }

        rows.Reverse();
        page.Messages.AddRange(rows.Select(ToProtocol));
        return page;
    }

    private static ChatMessage ToProtocol(Data.Message message) => new()
    {
        Id = message.Id,
        Author = message.Author,
        Text = message.Text,
        SentAtUnixMs = new DateTimeOffset(message.SentAt).ToUnixTimeMilliseconds(),
    };
}
