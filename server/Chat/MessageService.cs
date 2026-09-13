using System.Text;
using Microsoft.EntityFrameworkCore;
using Npgsql;
using Vorcall.Server.Attachments;
using Vorcall.Server.Data;
using Vorcall.Server.Metrics;
using Vorcall.Server.Protocol;
using Vorcall.Server.Streams;

namespace Vorcall.Server.Chat;

// Message is set only when Status is Appended.
public sealed record AppendOutcome(AppendOutcome.Kind Status, ChatMessage? Message)
{
    public enum Kind
    {
        Appended,
        UnknownReply,
        InvalidAttachment,
        InvalidStream,
    }

    public static AppendOutcome UnknownReply { get; } = new(Kind.UnknownReply, null);

    public static AppendOutcome InvalidAttachment { get; } = new(Kind.InvalidAttachment, null);

    public static AppendOutcome InvalidStream { get; } = new(Kind.InvalidStream, null);

    public static AppendOutcome Appended(ChatMessage message) => new(Kind.Appended, message);
}

// ChannelId is 0 only when Status is Unknown; Message is set only when Status is Edited.
public sealed record EditOutcome(EditOutcome.Kind Status, long ChannelId, ChatMessage? Message)
{
    public enum Kind
    {
        Edited,
        Unknown,
        Forbidden,
    }

    public static EditOutcome Unknown { get; } = new(Kind.Unknown, 0, null);

    public static EditOutcome Forbidden(long channelId) => new(Kind.Forbidden, channelId, null);

    public static EditOutcome Edited(long channelId, ChatMessage message) => new(Kind.Edited, channelId, message);
}

// Attachments are the rows the delete removed, so the caller can delete their files.
public sealed record DeleteOutcome(DeleteOutcome.Kind Status, long ChannelId, IReadOnlyList<(long Id, string ContentType)> Attachments)
{
    public enum Kind
    {
        Deleted,
        Unknown,
        Forbidden,
    }

    public static DeleteOutcome Unknown { get; } = new(Kind.Unknown, 0, []);

    public static DeleteOutcome Forbidden(long channelId) => new(Kind.Forbidden, channelId, []);

    public static DeleteOutcome Deleted(long channelId, IReadOnlyList<(long Id, string ContentType)> attachments)
        => new(Kind.Deleted, channelId, attachments);
}

// Reactions is the full grouped set for the message, which the broadcast carries as-is.
public sealed record ReactOutcome(ReactOutcome.Kind Status, long ChannelId, IReadOnlyList<Protocol.Reaction> Reactions)
{
    public enum Kind
    {
        Changed,
        Unknown,
    }

    public static ReactOutcome Unknown { get; } = new(Kind.Unknown, 0, []);

    public static ReactOutcome Changed(long channelId, IReadOnlyList<Protocol.Reaction> reactions)
        => new(Kind.Changed, channelId, reactions);
}

public sealed class MessageService(IDbContextFactory<AppDbContext> contextFactory, ServerMetrics metrics)
{
    private const int ExcerptMaxScalars = 120;

    // mayMentionEveryone is MENTION_EVERYONE resolved for the sender in this channel: without it
    // the literal words stay plain text and both flags are false, rather than the message being
    // refused.
    public async Task<AppendOutcome> AppendAsync(
        long userId,
        string author,
        long channelId,
        string text,
        long replyToId,
        IReadOnlyList<long> attachmentIds,
        IReadOnlyList<long> streamedFileIds,
        bool mayMentionEveryone)
    {
        // The handler checks both before it gets here; a service that trusts its caller is a
        // service that writes a message with someone else's file attached.
        if (attachmentIds.Count > AttachmentsOptions.MaxPerMessage
            || attachmentIds.Distinct().Count() != attachmentIds.Count)
        {
            return AppendOutcome.InvalidAttachment;
        }

        if (streamedFileIds.Count > StreamOptions.MaxPerMessage
            || streamedFileIds.Distinct().Count() != streamedFileIds.Count)
        {
            return AppendOutcome.InvalidStream;
        }

        await using var db = await contextFactory.CreateDbContextAsync();

        // The message and the links to its attachments land together: an attachment linked to a
        // message that was never written would be swept as unlinked and lose its file.
        await using var transaction = await db.Database.BeginTransactionAsync();

        ReplyTarget? replyTarget = null;
        if (replyToId != 0)
        {
            // A tombstone is a legal target: it keeps its id and author precisely so replies
            // still resolve.
            replyTarget = await db.Messages
                .AsNoTracking()
                .Where(m => m.Id == replyToId && m.ChannelId == channelId)
                .Select(m => new ReplyTarget(m.Id, m.Author, m.Text, m.DeletedAt != null))
                .FirstOrDefaultAsync();
            if (replyTarget is null)
            {
                return AppendOutcome.UnknownReply;
            }
        }

        var mentionEveryone = false;
        var mentionHere = false;
        if (mayMentionEveryone)
        {
            (mentionEveryone, mentionHere) = Validation.MentionFlags(text);
        }

        // Author is stored next to the id: the message keeps the name it was sent under even
        // if the account is renamed or deleted.
        var message = new Data.Message
        {
            UserId = userId,
            Author = author,
            ChannelId = channelId,
            Text = text,
            SentAt = DateTime.UtcNow,
            ReplyToId = replyToId == 0 ? null : replyToId,
            MentionIds = await Mentions.ResolveAsync(db, text),
            MentionEveryone = mentionEveryone,
            MentionHere = mentionHere,
        };

        db.Messages.Add(message);
        await db.SaveChangesAsync();

        var attachments = new List<Data.Attachment>();
        if (attachmentIds.Count > 0)
        {
            var ids = attachmentIds.ToList();

            // One guarded update rather than a read and then a write: MessageId == null inside
            // the predicate is what makes the claim atomic, so a second SendMessage naming the
            // same upload, or the sweeper deleting it, loses instead of racing. It also keeps a
            // row that vanished under us out of the change tracker, where it would surface as a
            // DbUpdateConcurrencyException and take the socket down with it. Complete keeps out
            // a row whose bytes are still arriving, or never finished: the row exists before the
            // body does, and this is the one place that must not take that on trust.
            var linked = await db.Attachments
                .Where(a => ids.Contains(a.Id) && a.MessageId == null && a.UploaderId == userId && a.ChannelId == channelId && a.Complete)
                .ExecuteUpdateAsync(setters => setters.SetProperty(a => a.MessageId, message.Id));
            if (linked != attachmentIds.Count)
            {
                // The message row goes with them: an append that cannot carry its attachments is
                // not a message the sender asked to send.
                await transaction.RollbackAsync();
                return AppendOutcome.InvalidAttachment;
            }

            attachments = await db.Attachments
                .AsNoTracking()
                .Where(a => ids.Contains(a.Id))
                .ToListAsync();
        }

        var streamedFiles = new List<Data.StreamedFile>();
        if (streamedFileIds.Count > 0)
        {
            var streamIds = streamedFileIds.ToList();

            // The same guarded claim, with the owner for the uploader. Nothing about bytes: a
            // streamed file has none here to have arrived.
            var linked = await db.StreamedFiles
                .Where(s => streamIds.Contains(s.Id) && s.MessageId == null && s.OwnerId == userId && s.ChannelId == channelId)
                .ExecuteUpdateAsync(setters => setters.SetProperty(s => s.MessageId, message.Id));
            if (linked != streamedFileIds.Count)
            {
                await transaction.RollbackAsync();
                return AppendOutcome.InvalidStream;
            }

            streamedFiles = await db.StreamedFiles
                .AsNoTracking()
                .Where(s => streamIds.Contains(s.Id))
                .ToListAsync();
        }

        await transaction.CommitAsync();
        metrics.CountMessage();

        // The client's order, not the database's: the sender chose it.
        var ordered = attachmentIds
            .Select(id => ToProtocol(attachments.First(a => a.Id == id)))
            .ToList();
        var orderedStreams = streamedFileIds
            .Select(id => StreamDirectory.ToProtocol(streamedFiles.First(s => s.Id == id)))
            .ToList();
        return AppendOutcome.Appended(ToProtocol(message, [], ordered, orderedStreams, replyTarget));
    }

    public async Task<EditOutcome> EditAsync(long id, long userId, string text, bool mayMentionEveryone)
    {
        await using var db = await contextFactory.CreateDbContextAsync();
        var message = await db.Messages.FirstOrDefaultAsync(m => m.Id == id);
        if (message is null || message.DeletedAt is not null)
        {
            return EditOutcome.Unknown;
        }

        // MANAGE_MESSAGES does not grant editing someone else's text, so the author is the only
        // one who gets past here.
        if (message.UserId != userId)
        {
            return EditOutcome.Forbidden(message.ChannelId);
        }

        var mentionEveryone = false;
        var mentionHere = false;
        if (mayMentionEveryone)
        {
            (mentionEveryone, mentionHere) = Validation.MentionFlags(text);
        }

        message.Text = text;
        message.EditedAt = DateTime.UtcNow;
        message.MentionIds = await Mentions.ResolveAsync(db, text);
        message.MentionEveryone = mentionEveryone;
        message.MentionHere = mentionHere;
        await db.SaveChangesAsync();

        var reactions = await LoadReactionsAsync(db, [id]);
        var attachments = await LoadAttachmentsAsync(db, [id]);
        var streamedFiles = await LoadStreamedFilesAsync(db, [id]);
        var replyTargets = await LoadReplyTargetsAsync(db, ReplyTargetIds([message]));
        return EditOutcome.Edited(
            message.ChannelId,
            ToProtocol(
                message,
                reactions.GetValueOrDefault(id, []),
                attachments.GetValueOrDefault(id, []),
                streamedFiles.GetValueOrDefault(id, []),
                ReplyTargetOf(message, replyTargets)));
    }

    // canManage is MANAGE_MESSAGES resolved for the actor in the message's channel: the author
    // may always delete its own, anyone else needs the bit.
    public async Task<DeleteOutcome> DeleteAsync(long id, long actorId, bool canManage)
    {
        await using var db = await contextFactory.CreateDbContextAsync();
        await using var transaction = await db.Database.BeginTransactionAsync();

        var message = await db.Messages.FirstOrDefaultAsync(m => m.Id == id);
        if (message is null || message.DeletedAt is not null)
        {
            return DeleteOutcome.Unknown;
        }

        if (message.UserId != actorId && !canManage)
        {
            return DeleteOutcome.Forbidden(message.ChannelId);
        }

        var attachments = await db.Attachments
            .AsNoTracking()
            .Where(a => a.MessageId == id)
            .OrderBy(a => a.Id)
            .Select(a => new { a.Id, a.ContentType })
            .ToListAsync();

        // The row survives as a tombstone: its id, author and channel are what a reply to it
        // still resolves against. ReplyToId is kept for the same reason, from the other side.
        message.DeletedAt = DateTime.UtcNow;
        message.Text = string.Empty;
        message.MentionIds = [];
        message.MentionEveryone = false;
        message.MentionHere = false;
        await db.SaveChangesAsync();
        await db.Reactions.Where(r => r.MessageId == id).ExecuteDeleteAsync();
        await db.Attachments.Where(a => a.MessageId == id).ExecuteDeleteAsync();
        await db.StreamedFiles.Where(s => s.MessageId == id).ExecuteDeleteAsync();
        await transaction.CommitAsync();

        return DeleteOutcome.Deleted(
            message.ChannelId,
            attachments.Select(a => (a.Id, a.ContentType)).ToList());
    }

    public async Task<ReactOutcome> ReactAsync(long messageId, long userId, string emoji, bool remove)
    {
        await using var db = await contextFactory.CreateDbContextAsync();
        var message = await db.Messages
            .AsNoTracking()
            .Where(m => m.Id == messageId)
            .Select(m => new { m.ChannelId, m.DeletedAt })
            .FirstOrDefaultAsync();
        if (message is null || message.DeletedAt is not null)
        {
            return ReactOutcome.Unknown;
        }

        if (remove)
        {
            await db.Reactions
                .Where(r => r.MessageId == messageId && r.UserId == userId && r.Emoji == emoji)
                .ExecuteDeleteAsync();
        }
        else if (!await db.Reactions.AnyAsync(r => r.MessageId == messageId && r.UserId == userId && r.Emoji == emoji))
        {
            db.Reactions.Add(new Data.Reaction
            {
                MessageId = messageId,
                UserId = userId,
                Emoji = emoji,
                CreatedAt = DateTime.UtcNow,
            });

            try
            {
                await db.SaveChangesAsync();
            }
            catch (DbUpdateException ex) when (ex.InnerException is PostgresException { SqlState: PostgresErrorCodes.UniqueViolation })
            {
                // The same reaction raced in from another connection of this account; adding one
                // that is already there is the documented no-op either way.
            }
        }

        var reactions = await LoadReactionsAsync(db, [messageId]);
        return ReactOutcome.Changed(message.ChannelId, reactions.GetValueOrDefault(messageId, []));
    }

    // Ids are global, not per channel: the latest id is what a client compares its history
    // against.
    public async Task<long> GetLatestIdAsync()
    {
        await using var db = await contextFactory.CreateDbContextAsync();
        return await db.Messages.MaxAsync(m => (long?)m.Id) ?? 0;
    }

    // Null when the id names no message, which every caller answers as an unknown message.
    public async Task<long?> ChannelOfAsync(long messageId)
    {
        await using var db = await contextFactory.CreateDbContextAsync();
        return await db.Messages
            .AsNoTracking()
            .Where(m => m.Id == messageId)
            .Select(m => (long?)m.ChannelId)
            .FirstOrDefaultAsync();
    }

    public async Task<MessagePage> GetPageAsync(long channelId, int limit, long? before)
    {
        await using var db = await contextFactory.CreateDbContextAsync();

        // Tombstones stay in the page: a history with holes would renumber what the client sees.
        var query = db.Messages.AsNoTracking().Where(m => m.ChannelId == channelId);
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

        var ids = rows.Select(m => m.Id).ToList();
        var reactions = await LoadReactionsAsync(db, ids);
        var attachments = await LoadAttachmentsAsync(db, ids);
        var streamedFiles = await LoadStreamedFilesAsync(db, ids);
        var replyTargets = await LoadReplyTargetsAsync(db, ReplyTargetIds(rows));

        page.Messages.AddRange(rows.Select(m => ToProtocol(
            m,
            reactions.GetValueOrDefault(m.Id, []),
            attachments.GetValueOrDefault(m.Id, []),
            streamedFiles.GetValueOrDefault(m.Id, []),
            ReplyTargetOf(m, replyTargets))));
        return page;
    }

    private static List<long> ReplyTargetIds(IReadOnlyList<Data.Message> messages)
        => messages.Select(m => m.ReplyToId).OfType<long>().Distinct().ToList();

    private static ReplyTarget? ReplyTargetOf(Data.Message message, IReadOnlyDictionary<long, ReplyTarget> targets)
        => message.ReplyToId is { } replyToId ? targets.GetValueOrDefault(replyToId) : null;

    private static async Task<Dictionary<long, ReplyTarget>> LoadReplyTargetsAsync(AppDbContext db, IReadOnlyList<long> ids)
    {
        if (ids.Count == 0)
        {
            return [];
        }

        return await db.Messages
            .AsNoTracking()
            .Where(m => ids.Contains(m.Id))
            .Select(m => new ReplyTarget(m.Id, m.Author, m.Text, m.DeletedAt != null))
            .ToDictionaryAsync(target => target.Id);
    }

    private static async Task<Dictionary<long, IReadOnlyList<Protocol.Reaction>>> LoadReactionsAsync(
        AppDbContext db,
        IReadOnlyList<long> messageIds)
    {
        if (messageIds.Count == 0)
        {
            return [];
        }

        // Ordered by when each row was written, so grouping in memory yields the emoji in the
        // order they were first used on the message.
        var rows = await db.Reactions
            .AsNoTracking()
            .Where(r => messageIds.Contains(r.MessageId))
            .OrderBy(r => r.CreatedAt)
            .ThenBy(r => r.UserId)
            .Select(r => new { r.MessageId, r.Emoji, r.UserId })
            .ToListAsync();

        return rows
            .GroupBy(r => r.MessageId)
            .ToDictionary(
                byMessage => byMessage.Key,
                byMessage => (IReadOnlyList<Protocol.Reaction>)byMessage
                    .GroupBy(r => r.Emoji)
                    .Select(byEmoji =>
                    {
                        var reaction = new Protocol.Reaction { Emoji = byEmoji.Key };
                        reaction.UserIds.AddRange(byEmoji.Select(r => r.UserId).OrderBy(id => id));
                        return reaction;
                    })
                    .ToList());
    }

    private static async Task<Dictionary<long, IReadOnlyList<Protocol.Attachment>>> LoadAttachmentsAsync(
        AppDbContext db,
        IReadOnlyList<long> messageIds)
    {
        if (messageIds.Count == 0)
        {
            return [];
        }

        var linked = messageIds.Select(id => (long?)id).ToList();
        var rows = await db.Attachments
            .AsNoTracking()
            .Where(a => linked.Contains(a.MessageId))
            .OrderBy(a => a.Id)
            .ToListAsync();

        return rows
            .GroupBy(a => a.MessageId!.Value)
            .ToDictionary(
                byMessage => byMessage.Key,
                byMessage => (IReadOnlyList<Protocol.Attachment>)byMessage.Select(ToProtocol).ToList());
    }

    private static async Task<Dictionary<long, IReadOnlyList<Protocol.StreamedFile>>> LoadStreamedFilesAsync(
        AppDbContext db,
        IReadOnlyList<long> messageIds)
    {
        if (messageIds.Count == 0)
        {
            return [];
        }

        var linked = messageIds.Select(id => (long?)id).ToList();
        var rows = await db.StreamedFiles
            .AsNoTracking()
            .Where(s => linked.Contains(s.MessageId))
            .OrderBy(s => s.Id)
            .ToListAsync();

        return rows
            .GroupBy(s => s.MessageId!.Value)
            .ToDictionary(
                byMessage => byMessage.Key,
                byMessage => (IReadOnlyList<Protocol.StreamedFile>)byMessage.Select(StreamDirectory.ToProtocol).ToList());
    }

    private static Protocol.Attachment ToProtocol(Data.Attachment attachment) => new()
    {
        Id = attachment.Id,
        FileName = attachment.FileName,
        ContentType = attachment.ContentType,
        Size = attachment.Size,
    };

    private static ChatMessage ToProtocol(
        Data.Message message,
        IReadOnlyList<Protocol.Reaction> reactions,
        IReadOnlyList<Protocol.Attachment> attachments,
        IReadOnlyList<Protocol.StreamedFile> streamedFiles,
        ReplyTarget? replyTarget)
    {
        var chatMessage = new ChatMessage
        {
            Id = message.Id,
            Author = message.Author,
            Text = message.Text,
            SentAtUnixMs = new DateTimeOffset(message.SentAt).ToUnixTimeMilliseconds(),
            ChannelId = message.ChannelId,

            // 0 for the messages that predate accounts, as PROTOCOL.md promises.
            AuthorId = message.UserId ?? 0,
            EditedAtUnixMs = message.EditedAt is { } editedAt
                ? new DateTimeOffset(editedAt).ToUnixTimeMilliseconds()
                : 0,
            Deleted = message.DeletedAt is not null,
            MentionEveryone = message.MentionEveryone,
            MentionHere = message.MentionHere,
        };

        chatMessage.MentionIds.AddRange(message.MentionIds);
        chatMessage.Reactions.AddRange(reactions);
        chatMessage.Attachments.AddRange(attachments);
        chatMessage.StreamedFiles.AddRange(streamedFiles);

        if (message.ReplyToId is { } replyToId)
        {
            // A target that no longer exists cannot happen — deleting tombstones rather than
            // removes — but the reference has to render as something, so it renders as deleted.
            chatMessage.ReplyTo = replyTarget is null
                ? new ReplyRef { Id = replyToId, Author = string.Empty, Deleted = true }
                : new ReplyRef
                {
                    Id = replyTarget.Id,
                    Author = replyTarget.Author,
                    Excerpt = Excerpt(replyTarget.Text),
                    Deleted = replyTarget.Deleted,
                };
        }

        return chatMessage;
    }

    private static string Excerpt(string text)
    {
        var scalars = 0;
        var length = 0;
        foreach (var rune in text.EnumerateRunes())
        {
            if (scalars == ExcerptMaxScalars)
            {
                return text[..length];
            }

            scalars++;
            length += rune.Utf16SequenceLength;
        }

        return text;
    }

    // The state a ReplyRef is rendered from: the target's current text and tombstone flag, read
    // when the reply is read rather than frozen when it was written.
    private sealed record ReplyTarget(long Id, string Author, string Text, bool Deleted);
}
