using Microsoft.EntityFrameworkCore;
using Vorcall.Server.Data;
using Vorcall.Server.Protocol;

namespace Vorcall.Server.Chat;

public sealed class MessageService(IDbContextFactory<AppDbContext> contextFactory)
{
    public async Task<ChatMessage> AppendAsync(long userId, string author, string roomId, string text)
    {
        await using var db = await contextFactory.CreateDbContextAsync();

        // Author is stored next to the id: the message keeps the name it was sent under even
        // if the account is renamed or deleted.
        var message = new Data.Message
        {
            UserId = userId,
            Author = author,
            RoomId = roomId,
            Text = text,
            SentAt = DateTime.UtcNow,
        };

        db.Messages.Add(message);
        await db.SaveChangesAsync();
        return ToProtocol(message);
    }

    // Ids are global, not per room: the latest id is what a client compares its history against.
    public async Task<long> GetLatestIdAsync()
    {
        await using var db = await contextFactory.CreateDbContextAsync();
        return await db.Messages.MaxAsync(m => (long?)m.Id) ?? 0;
    }

    public async Task<MessagePage> GetPageAsync(string roomId, int limit, long? before)
    {
        await using var db = await contextFactory.CreateDbContextAsync();
        var query = db.Messages.AsNoTracking().Where(m => m.RoomId == roomId);
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
        RoomId = message.RoomId,

        // 0 for the messages that predate accounts, as PROTOCOL.md promises.
        AuthorId = message.UserId ?? 0,
    };
}
