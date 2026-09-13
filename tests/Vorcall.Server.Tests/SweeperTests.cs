using System.Net;
using Microsoft.EntityFrameworkCore;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.Hosting;
using Vorcall.Server.Attachments;
using Vorcall.Server.Auth;
using Vorcall.Server.Data;
using Vorcall.Server.Streams;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;

namespace Vorcall.Server.Tests;

[Collection(ServerCollection.Name)]
public sealed class SweeperTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Sweep_deletes_only_rows_seven_days_past_expiry_or_revocation()
    {
        var dave = await Accounts.RegisterAsync(Server, "sweepe");
        var now = DateTime.UtcNow;
        var contexts = Server.Services.GetRequiredService<IDbContextFactory<AppDbContext>>();

        long live, expiredLongAgo, revokedLongAgo, expiredRecently, revokedRecently;
        await using (var db = await contexts.CreateDbContextAsync())
        {
            live = await db.RefreshTokens.Where(t => t.UserId == dave.UserId).Select(t => t.Id).SingleAsync();

            var rows = new[]
            {
                Row(dave.UserId, now, expiresAt: now.AddDays(-8), revokedAt: null),
                Row(dave.UserId, now, expiresAt: now.AddDays(30), revokedAt: now.AddDays(-8)),
                Row(dave.UserId, now, expiresAt: now.AddDays(-6), revokedAt: null),
                Row(dave.UserId, now, expiresAt: now.AddDays(30), revokedAt: now.AddDays(-6)),
            };
            db.RefreshTokens.AddRange(rows);
            await db.SaveChangesAsync();
            expiredLongAgo = rows[0].Id;
            revokedLongAgo = rows[1].Id;
            expiredRecently = rows[2].Id;
            revokedRecently = rows[3].Id;
        }

        var sweeper = Server.Services.GetServices<IHostedService>().OfType<RefreshTokenSweeper>().Single();
        var removed = await sweeper.SweepAsync(now);
        Assert.True(removed >= 2, $"swept {removed} rows");

        await using (var db = await contexts.CreateDbContextAsync())
        {
            var remaining = await db.RefreshTokens
                .Where(t => t.UserId == dave.UserId)
                .Select(t => t.Id)
                .ToListAsync();
            Assert.DoesNotContain(expiredLongAgo, remaining);
            Assert.DoesNotContain(revokedLongAgo, remaining);
            Assert.Contains(expiredRecently, remaining);
            Assert.Contains(revokedRecently, remaining);
            Assert.Contains(live, remaining);
        }

        await Accounts.RefreshAsync(Server, dave);
    }

    [Fact]
    public void Grace_is_seven_days()
    {
        Assert.Equal(TimeSpan.FromDays(7), RefreshTokenSweeper.Grace);
    }

    [Fact]
    public async Task An_unlinked_upload_is_swept_when_it_is_stale_and_an_unfinished_one_waits_a_day()
    {
        var store = Server.Services.GetRequiredService<AttachmentStore>();
        var contexts = Server.Services.GetRequiredService<IDbContextFactory<AppDbContext>>();
        var mia = await Accounts.RegisterAsync(Server, "zaira");
        var channel = await GeneralIdAsync(mia);
        var now = DateTime.UtcNow;

        // Two hours old and unlinked, one with its bytes on disk and one still without them.
        var stale = await UploadAsync(contexts, store, mia.UserId, channel, complete: true, createdAt: now.AddHours(-2));
        var unfinished = await UploadAsync(contexts, store, mia.UserId, channel, complete: false, createdAt: now.AddHours(-2));

        var swept = await store.SweepUnlinkedAsync(now.AddHours(-1), now.AddHours(-24));
        Assert.True(swept >= 1, $"swept {swept} rows");

        await using (var db = await contexts.CreateDbContextAsync())
        {
            Assert.Null(await db.Attachments.AsNoTracking().FirstOrDefaultAsync(a => a.Id == stale));

            // A large upload over a slow link outlives the short cutoff several times over, so an
            // incomplete row is not stale until the long one.
            Assert.NotNull(await db.Attachments.AsNoTracking().FirstOrDefaultAsync(a => a.Id == unfinished));
        }

        Assert.False(File.Exists(store.PathFor(stale)), "a swept upload left its file behind");
        Assert.True(File.Exists(store.PathFor(unfinished) + ".part"), "an upload still in flight lost its bytes");

        // The same row a day later.
        await using (var db = await contexts.CreateDbContextAsync())
        {
            var row = await db.Attachments.SingleAsync(a => a.Id == unfinished);
            row.CreatedAt = now.AddHours(-25);
            await db.SaveChangesAsync();
        }

        Assert.True(await store.SweepUnlinkedAsync(now.AddHours(-1), now.AddHours(-24)) >= 1);
        await using (var db = await contexts.CreateDbContextAsync())
        {
            Assert.Null(await db.Attachments.AsNoTracking().FirstOrDefaultAsync(a => a.Id == unfinished));
        }

        // An incomplete row's bytes are under the .part name and nothing is left to find them by
        // once its row is gone, so the sweep has to take that name too.
        Assert.False(File.Exists(store.PathFor(unfinished) + ".part"), "a swept upload left a .part behind");
        Assert.False(File.Exists(store.PathFor(unfinished)));
    }

    [Fact]
    public async Task A_linked_upload_is_never_swept_however_old_it_is()
    {
        var store = Server.Services.GetRequiredService<AttachmentStore>();
        var contexts = Server.Services.GetRequiredService<IDbContextFactory<AppDbContext>>();
        await using var party = await Party.ConnectAsync(Server, "greta");
        var (greta, g1) = (party.Account(0), party.Client(0));
        var general = party.GeneralId;

        var uploaded = await Uploads.UploadAsync(Server, greta.Access, general, Blob.Of(128), "application/octet-stream", "kept.bin");
        Assert.Equal(HttpStatusCode.Created, uploaded.Status);
        var id = uploaded.As(Vorcall.Server.Protocol.Attachment.Parser).Id;

        var text = $"kept {Names.Token()}";
        await g1.SendAsync(Frames.Send(text, general, 0, id));
        await party.ExpectMessageEverywhereAsync(text, general);

        await using (var db = await contexts.CreateDbContextAsync())
        {
            var row = await db.Attachments.SingleAsync(a => a.Id == id);
            row.CreatedAt = DateTime.UtcNow.AddDays(-30);
            await db.SaveChangesAsync();
        }

        await store.SweepUnlinkedAsync(DateTime.UtcNow, DateTime.UtcNow);
        await using (var db = await contexts.CreateDbContextAsync())
        {
            Assert.NotNull(await db.Attachments.AsNoTracking().FirstOrDefaultAsync(a => a.Id == id));
        }

        Assert.True(File.Exists(store.PathFor(id)));
    }

    [Fact]
    public async Task An_offer_nothing_ever_linked_is_swept_and_a_linked_one_is_left_where_it_is()
    {
        var streams = Server.Services.GetRequiredService<StreamDirectory>();
        var contexts = Server.Services.GetRequiredService<IDbContextFactory<AppDbContext>>();
        await using var party = await Party.ConnectAsync(Server, "hedda");
        var (hedda, h1) = (party.Account(0), party.Client(0));
        var general = party.GeneralId;

        // A client that offers a file and then abandons the composer leaves this behind; there is
        // no file to reclaim, but the row would otherwise name an owner and a channel for good.
        var abandoned = await OfferAsync(hedda, general);
        var kept = await OfferAsync(hedda, general);

        var text = $"kept {Names.Token()}";
        await h1.SendAsync(Frames.SendStreamed(text, general, kept.Id));
        await party.ExpectMessageEverywhereAsync(text, general);

        var now = DateTime.UtcNow;
        await using (var db = await contexts.CreateDbContextAsync())
        {
            var ids = new[] { abandoned.Id, kept.Id };
            foreach (var row in await db.StreamedFiles.Where(s => ids.Contains(s.Id)).ToListAsync())
            {
                row.CreatedAt = now.AddHours(-2);
            }

            await db.SaveChangesAsync();
        }

        Assert.True(await streams.SweepUnlinkedAsync(now.AddHours(-1), CancellationToken.None) >= 1);

        await using (var db = await contexts.CreateDbContextAsync())
        {
            Assert.Null(await db.StreamedFiles.AsNoTracking().FirstOrDefaultAsync(s => s.Id == abandoned.Id));
            Assert.NotNull(await db.StreamedFiles.AsNoTracking().FirstOrDefaultAsync(s => s.Id == kept.Id));
        }
    }

    private async Task<Vorcall.Server.Protocol.StreamedFile> OfferAsync(Account owner, long channelId)
    {
        var response = await StreamApi.OfferAsync(
            Server,
            owner.Access,
            channelId,
            new Vorcall.Server.Protocol.StreamOffer { FileName = "old.bin", ContentType = StreamApi.Octets, Size = 4096 });
        Assert.True(response.Status == HttpStatusCode.Created, $"offer: {(int)response.Status}");
        return response.As(Vorcall.Server.Protocol.StreamedFile.Parser);
    }

    // The general channel's id, which only a snapshot names; the socket is gone by the time the
    // caller uses it.
    private async Task<long> GeneralIdAsync(Account account)
    {
        await using var client = await WsClient.ConnectAsync(Server, account);
        return client.Session.GeneralId;
    }

    // A row and the bytes that go with it, written the way an upload leaves them: a complete one
    // under <id>.bin, an unfinished one under <id>.bin.part.
    private static async Task<long> UploadAsync(
        IDbContextFactory<AppDbContext> contexts,
        AttachmentStore store,
        long uploaderId,
        long channelId,
        bool complete,
        DateTime createdAt)
    {
        await using var db = await contexts.CreateDbContextAsync();
        var row = new Attachment
        {
            ChannelId = channelId,
            UploaderId = uploaderId,
            FileName = "old.bin",
            ContentType = "application/octet-stream",
            Size = 64,
            Complete = complete,
            CreatedAt = createdAt,
        };
        db.Attachments.Add(row);
        await db.SaveChangesAsync();

        Directory.CreateDirectory(Path.GetDirectoryName(store.PathFor(row.Id))!);
        await File.WriteAllBytesAsync(store.PathFor(row.Id) + (complete ? string.Empty : ".part"), Blob.Of(64));
        return row.Id;
    }

    private static RefreshToken Row(long userId, DateTime createdAt, DateTime expiresAt, DateTime? revokedAt) => new()
    {
        UserId = userId,
        TokenHash = Credentials.Sha256Hex(Guid.NewGuid().ToString("N")),
        FamilyId = Guid.NewGuid(),
        CreatedAt = createdAt,
        LastUsedAt = createdAt,
        ExpiresAt = expiresAt,
        RevokedAt = revokedAt,
    };
}
