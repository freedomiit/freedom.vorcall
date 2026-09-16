using System.Net;
using Microsoft.EntityFrameworkCore;
using Microsoft.Extensions.DependencyInjection;
using Vorcall.Server.Api;
using Vorcall.Server.Attachments;
using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;
using Vorcall.Server.Tests.Infrastructure;
using Xunit;
using Kind = Vorcall.Server.Protocol.ServerFrame.PayloadOneofCase;

namespace Vorcall.Server.Tests;

// One shared sticker library every member sees whole, uploaded and read back over REST, managed
// over the socket, and sent as a message of its own.
[Collection(ServerCollection.Name)]
public sealed class StickersTests(ServerFixture fixture)
{
    private VorcallFactory Server => fixture.Server;

    [Fact]
    public async Task Only_MANAGE_STICKERS_may_add_a_sticker_and_everyone_hears_of_it()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "stella");
        var (owner, stella) = (party.Account(0), party.Account(1));
        var pixel = Png.Pixel();

        // MANAGE_STICKERS is no @everyone default.
        var refused = await Uploads.UploadStickerAsync(Server, stella.Access, "wave", pixel);
        Assert.Equal(HttpStatusCode.Forbidden, refused.Status);
        Assert.Equal("MANAGE_STICKERS", refused.Detail);

        var uploaded = await Uploads.UploadStickerAsync(Server, owner.Access, "  wave  ", pixel);
        Assert.Equal(HttpStatusCode.Created, uploaded.Status);
        var sticker = uploaded.As(Sticker.Parser);
        Assert.True(sticker.Id > 0);
        Assert.Equal("wave", sticker.Name);
        Assert.Equal(owner.UserId, sticker.UploaderId);
        Assert.Equal("image/png", sticker.ContentType);
        Assert.Equal((long)pixel.Length, sticker.Size);

        // Server-wide, so the delta goes to everyone, the uploader included.
        foreach (var client in party.Clients)
        {
            var upserted = (await client.ExpectAsync(Kind.StickerUpserted)).StickerUpserted.Sticker;
            Assert.Equal(sticker.Id, upserted.Id);
            Assert.Equal("wave", upserted.Name);
            Assert.Equal(owner.UserId, upserted.UploaderId);
            Assert.Equal("image/png", upserted.ContentType);
            Assert.Equal((long)pixel.Length, upserted.Size);
        }

        // A member holding the bit outright, rather than the owner's bypass, may add one too.
        await party.GrantRoleAsync(1, (ulong)Perm.ManageStickers);
        var granted = await Uploads.UploadStickerAsync(Server, stella.Access, "hi", pixel);
        Assert.Equal(HttpStatusCode.Created, granted.Status);
        await ConsumeUpsertedAsync(party, granted.As(Sticker.Parser).Id);
    }

    [Fact]
    public async Task The_upload_refuses_a_foreign_type_a_mismatched_magic_an_oversized_and_a_lengthless_body()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture);
        var owner = party.Account(0);
        var pixel = Png.Pixel();

        foreach (var type in new[] { "image/bmp", "image/svg+xml", "application/octet-stream" })
        {
            var mistyped = await Uploads.UploadStickerAsync(Server, owner.Access, "wrongtype", pixel, type);
            Assert.True(mistyped.Status == HttpStatusCode.UnsupportedMediaType, $"{type}: {(int)mistyped.Status}");
        }

        // PNG bytes declared as a GIF: the declared type is one of the four, the body is not it.
        var mismatched = await Uploads.UploadStickerAsync(Server, owner.Access, "liar", pixel, "image/gif");
        Assert.Equal(HttpStatusCode.BadRequest, mismatched.Status);
        Assert.Equal(AttachmentsEndpoints.NotAnImageDetail, mismatched.Detail);

        // Refused from the declared length alone: one byte past 1 MiB.
        var tooLarge = await Uploads.UploadStickerAsync(Server, owner.Access, "huge", pixel, declaredLength: (1L << 20) + 1);
        Assert.Equal(HttpStatusCode.RequestEntityTooLarge, tooLarge.Status);
        Assert.Equal(StickersEndpoints.TooLargeDetail, tooLarge.Detail);

        var lengthless = await Proto.SendAsync(
            Server,
            HttpMethod.Post,
            "/api/stickers?name=nolength",
            UnmeasurableContent(pixel),
            owner.Access,
            ServerFixture.ServerKey,
            request => request.Headers.TransferEncodingChunked = true);
        Assert.Equal(HttpStatusCode.LengthRequired, lengthless.Status);
        Assert.Equal(AttachmentsEndpoints.LengthRequiredDetail, lengthless.Detail);

        // Nothing above made it into the library.
        await party.Client(0).QuietAsync();
    }

    [Fact]
    public async Task The_upload_refuses_a_name_the_grammar_does_not_accept()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture);
        var owner = party.Account(0);

        foreach (var refusedName in new[] { new string('a', 33), "   ", string.Empty })
        {
            var refused = await Uploads.UploadStickerAsync(Server, owner.Access, refusedName, Png.Pixel());
            Assert.True(refused.Status == HttpStatusCode.BadRequest, $"'{refusedName.Length}': {(int)refused.Status}");
            Assert.Equal("name", refused.Detail);
        }

        await party.Client(0).QuietAsync();
    }

    [Fact]
    public async Task The_upload_past_two_hundred_stickers_is_a_conflict()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture);
        var owner = party.Account(0);
        var contexts = Server.Services.GetRequiredService<IDbContextFactory<Data.AppDbContext>>();

        // Complete rows written straight into the table up to the ceiling of 200, whatever other
        // tests left there, and taken out again so no later upload trips over them.
        var seeded = new List<long>();
        try
        {
            await using (var db = await contexts.CreateDbContextAsync())
            {
                var existing = await db.Stickers.CountAsync(s => s.Complete);
                var rows = Enumerable.Range(0, Math.Max(0, 200 - existing))
                    .Select(i => new Data.Sticker
                    {
                        Name = $"filler {i}",
                        ContentType = "image/png",
                        Size = 1,
                        Complete = true,
                        CreatedAt = DateTime.UtcNow,
                    })
                    .ToList();
                db.Stickers.AddRange(rows);
                await db.SaveChangesAsync();
                seeded.AddRange(rows.Select(row => row.Id));
            }

            var refused = await Uploads.UploadStickerAsync(Server, owner.Access, "one too many", Png.Pixel());
            Assert.Equal(HttpStatusCode.Conflict, refused.Status);
            Assert.Equal(StickersEndpoints.LimitReachedDetail, refused.Detail);
            await party.Client(0).QuietAsync();
        }
        finally
        {
            await using var db = await contexts.CreateDbContextAsync();
            await db.Stickers.Where(s => seeded.Contains(s.Id)).ExecuteDeleteAsync();
        }
    }

    [Fact]
    public async Task A_sticker_is_read_back_by_any_bearer_with_the_id_as_its_ETag()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "tilda");
        var (owner, tilda) = (party.Account(0), party.Account(1));
        var pixel = Png.Pixel();
        var sticker = await UploadAsync(party, owner);

        var served = await Uploads.DownloadStickerAsync(Server, tilda.Access, sticker.Id);
        Assert.Equal(HttpStatusCode.OK, served.Status);
        Assert.Equal(pixel, served.Body);
        Assert.Equal("image/png", served.Header("Content-Type"));
        Assert.Equal($"\"{sticker.Id}\"", served.Header("ETag"));
        Assert.Equal(["immutable", "max-age=31536000", "private"], served.Parts("Cache-Control"));

        var missing = await Uploads.DownloadStickerAsync(Server, tilda.Access, sticker.Id + 1_000_000);
        Assert.Equal(HttpStatusCode.NotFound, missing.Status);
    }

    [Fact]
    public async Task The_hello_snapshot_carries_the_library_and_not_an_upload_that_never_finished()
    {
        var contexts = Server.Services.GetRequiredService<IDbContextFactory<Data.AppDbContext>>();
        var store = Server.Services.GetRequiredService<StickerStore>();
        var pixel = Png.Pixel();

        Sticker sticker;
        await using (var party = await Party.WithOwnerAsync(Server, fixture))
        {
            sticker = await UploadAsync(party, party.Account(0));
        }

        // The row a process killed mid-upload leaves behind, its file in place, so the flag is the
        // only thing refusing it.
        long halfId;
        await using (var db = await contexts.CreateDbContextAsync())
        {
            var row = new Data.Sticker
            {
                Name = "half",
                ContentType = "image/png",
                Size = pixel.LongLength,
                Complete = false,
                CreatedAt = DateTime.UtcNow,
            };
            db.Stickers.Add(row);
            await db.SaveChangesAsync();
            halfId = row.Id;
        }

        var path = store.PathFor(halfId);
        Directory.CreateDirectory(Path.GetDirectoryName(path)!);
        await File.WriteAllBytesAsync(path, pixel);

        var reader = await Accounts.RegisterAsync(Server, "ines");
        await using var client = await WsClient.ConnectAsync(Server, reader);

        var listed = Assert.Single(client.Session.Snapshot.Stickers, entry => entry.Id == sticker.Id);
        Assert.Equal(sticker.Name, listed.Name);
        Assert.Equal(sticker.UploaderId, listed.UploaderId);
        Assert.Equal("image/png", listed.ContentType);
        Assert.Equal(sticker.Size, listed.Size);
        Assert.DoesNotContain(client.Session.Snapshot.Stickers, entry => entry.Id == halfId);

        var served = await Uploads.DownloadStickerAsync(Server, reader.Access, halfId);
        Assert.Equal(HttpStatusCode.NotFound, served.Status);

        // The sweeper takes the row and its file once the one-hour cutoff has passed.
        Assert.True(await store.SweepIncompleteAsync(DateTime.UtcNow.AddHours(1), CancellationToken.None) >= 1);
        await using (var db = await contexts.CreateDbContextAsync())
        {
            Assert.Null(await db.Stickers.AsNoTracking().FirstOrDefaultAsync(s => s.Id == halfId));
        }

        Assert.False(File.Exists(path));
    }

    [Fact]
    public async Task Renaming_and_deleting_need_MANAGE_STICKERS_and_reach_everyone()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "berta");
        var (owner, o1, b1) = (party.Account(0), party.Client(0), party.Client(1));
        var sticker = await UploadAsync(party, owner);

        foreach (var frame in new[] { Frames.UpdateSticker(sticker.Id, "stolen"), Frames.DeleteSticker(sticker.Id) })
        {
            await b1.SendAsync(frame);
            var denied = await b1.ExpectErrorAsync(ErrorCode.PermissionDenied, fatal: false);
            Assert.Equal("MANAGE_STICKERS", denied.Detail);
        }

        await o1.SendAsync(Frames.UpdateSticker(sticker.Id + 1_000_000, "ghost"));
        await o1.ExpectErrorAsync(ErrorCode.UnknownSticker, fatal: false);
        await o1.SendAsync(Frames.DeleteSticker(sticker.Id + 1_000_000));
        await o1.ExpectErrorAsync(ErrorCode.UnknownSticker, fatal: false);

        await o1.SendAsync(Frames.UpdateSticker(sticker.Id, string.Empty));
        var badName = await o1.ExpectErrorAsync(ErrorCode.InvalidArgument, fatal: false);
        Assert.Equal("name", badName.Detail);

        await o1.SendAsync(Frames.UpdateSticker(sticker.Id, "  renamed  "));
        foreach (var client in party.Clients)
        {
            var upserted = (await client.ExpectAsync(Kind.StickerUpserted)).StickerUpserted.Sticker;
            Assert.Equal(sticker.Id, upserted.Id);
            Assert.Equal("renamed", upserted.Name);
            Assert.Equal(sticker.Size, upserted.Size);
        }

        await o1.SendAsync(Frames.DeleteSticker(sticker.Id));
        foreach (var client in party.Clients)
        {
            Assert.Equal(sticker.Id, (await client.ExpectAsync(Kind.StickerDeleted)).StickerDeleted.StickerId);
        }

        var gone = await Uploads.DownloadStickerAsync(Server, owner.Access, sticker.Id);
        Assert.Equal(HttpStatusCode.NotFound, gone.Status);
    }

    [Fact]
    public async Task A_sticker_message_is_broadcast_as_a_sticker_and_may_reply()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "celia");
        var (owner, c1) = (party.Account(0), party.Client(1));
        var general = party.GeneralId;
        var sticker = await UploadAsync(party, owner);

        // An ordinary member: SEND_MESSAGES is all a sticker needs, it is no attachment.
        await c1.SendAsync(Frames.SendSticker(general, sticker.Id));
        long stickerMessageId = 0;
        foreach (var client in party.Clients)
        {
            var message = (await client.ExpectAsync(Kind.Message)).Message;
            Assert.Equal(general, message.ChannelId);
            Assert.True(message.Sticker);
            Assert.Equal(sticker.Id, message.StickerId);
            Assert.Equal(string.Empty, message.Text);
            Assert.Empty(message.Attachments);
            stickerMessageId = message.Id;
        }

        await c1.SendAsync(Frames.SendSticker(general, sticker.Id, replyToId: stickerMessageId));
        foreach (var client in party.Clients)
        {
            var message = (await client.ExpectAsync(Kind.Message)).Message;
            Assert.True(message.Sticker);
            Assert.Equal(sticker.Id, message.StickerId);
            Assert.Equal(stickerMessageId, message.ReplyTo.Id);
        }

        var stored = (await History.ByIdAsync(Server, owner.Access, general))[stickerMessageId];
        Assert.True(stored.Sticker);
        Assert.Equal(sticker.Id, stored.StickerId);
    }

    [Fact]
    public async Task A_sticker_travels_alone_and_must_exist()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "dora");
        var (owner, d1) = (party.Account(0), party.Client(1));
        var dora = party.Account(1);
        var general = party.GeneralId;
        var sticker = await UploadAsync(party, owner);

        await d1.SendAsync(Frames.SendSticker(general, sticker.Id, text: "and words"));
        await d1.ExpectErrorAsync(ErrorCode.InvalidMessage, fatal: false);

        var upload = await Uploads.UploadAsync(Server, dora.Access, general, Png.Pixel());
        Assert.Equal(HttpStatusCode.Created, upload.Status);
        var attachment = upload.As(Attachment.Parser);
        await d1.SendAsync(Frames.SendSticker(general, sticker.Id, attachmentIds: [attachment.Id]));
        await d1.ExpectErrorAsync(ErrorCode.InvalidMessage, fatal: false);

        await d1.SendAsync(Frames.SendSticker(general, sticker.Id + 1_000_000));
        await d1.ExpectErrorAsync(ErrorCode.UnknownSticker, fatal: false);

        // Empty text with nothing else to carry is still refused as it always was.
        await d1.SendAsync(Frames.Send(string.Empty, general));
        await d1.ExpectErrorAsync(ErrorCode.InvalidMessage, fatal: false);

        foreach (var client in party.Clients)
        {
            await client.QuietAsync();
        }
    }

    [Fact]
    public async Task A_sticker_message_cannot_be_edited()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "gilda");
        var (owner, g1) = (party.Account(0), party.Client(1));
        var general = party.GeneralId;
        var sticker = await UploadAsync(party, owner);

        await g1.SendAsync(Frames.SendSticker(general, sticker.Id));
        var messageId = (await party.ExpectEverywhereAsync(Kind.Message))[0].Message.Id;

        await g1.SendAsync(Frames.Edit(messageId, "now with words"));
        var refused = await g1.ExpectErrorAsync(ErrorCode.InvalidMessage, fatal: false);
        Assert.Equal("a sticker message cannot be edited", refused.Detail);
        foreach (var client in party.Clients)
        {
            await client.QuietAsync();
        }

        var stored = (await History.ByIdAsync(Server, owner.Access, general))[messageId];
        Assert.True(stored.Sticker);
        Assert.Equal(sticker.Id, stored.StickerId);
        Assert.Equal(string.Empty, stored.Text);
        Assert.Equal(0L, stored.EditedAtUnixMs);
    }

    [Fact]
    public async Task A_sticker_message_needs_SEND_MESSAGES()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "elke");
        var (owner, e1) = (party.Account(0), party.Client(1));
        var sticker = await UploadAsync(party, owner);
        var (_, channelId) = await party.CreateChannelAsync();
        await party.SetRoleOverrideAsync(channelId, party.EveryoneId, deny: (ulong)Perm.SendMessages);

        await e1.SendAsync(Frames.SendSticker(channelId, sticker.Id));
        var denied = await e1.ExpectErrorAsync(ErrorCode.PermissionDenied, fatal: false);
        Assert.Equal("SEND_MESSAGES", denied.Detail);
    }

    [Fact]
    public async Task A_deleted_sticker_leaves_its_messages_flagged_and_a_tombstone_clears_both_fields()
    {
        await using var party = await Party.WithOwnerAsync(Server, fixture, "fenna");
        var (owner, o1, f1) = (party.Account(0), party.Client(0), party.Client(1));
        var general = party.GeneralId;
        var sticker = await UploadAsync(party, owner);
        var survivor = await UploadAsync(party, owner);

        await f1.SendAsync(Frames.SendSticker(general, sticker.Id));
        var orphanedId = (await party.ExpectEverywhereAsync(Kind.Message))[0].Message.Id;
        await f1.SendAsync(Frames.SendSticker(general, survivor.Id));
        var doomedId = (await party.ExpectEverywhereAsync(Kind.Message))[0].Message.Id;

        await o1.SendAsync(Frames.DeleteSticker(sticker.Id));
        await party.ExpectEverywhereAsync(Kind.StickerDeleted);

        var orphaned = (await History.ByIdAsync(Server, owner.Access, general))[orphanedId];
        Assert.True(orphaned.Sticker);
        Assert.Equal(0L, orphaned.StickerId);

        await f1.SendAsync(Frames.Delete(doomedId));
        await party.ExpectEverywhereAsync(Kind.MessageDeleted);

        var tombstone = (await History.ByIdAsync(Server, owner.Access, general))[doomedId];
        Assert.True(tombstone.Deleted);
        Assert.False(tombstone.Sticker);
        Assert.Equal(0L, tombstone.StickerId);
    }

    // A body whose length the client cannot work out; sent chunked, since a test host handed a
    // measurable body computes the header this endpoint has to do without.
    private static HttpContent UnmeasurableContent(byte[] body)
    {
        var content = new StreamContent(new UnmeasurableStream(body));
        content.Headers.ContentType = System.Net.Http.Headers.MediaTypeHeaderValue.Parse("image/png");
        return content;
    }

    // A sticker in the library, with the upsert it broadcast already taken off every inbox.
    private async Task<Sticker> UploadAsync(Party party, Account uploader)
    {
        var response = await Uploads.UploadStickerAsync(Server, uploader.Access, $"sticker {Names.Token()}", Png.Pixel());
        Assert.Equal(HttpStatusCode.Created, response.Status);
        var sticker = response.As(Sticker.Parser);
        await ConsumeUpsertedAsync(party, sticker.Id);
        return sticker;
    }

    private static async Task ConsumeUpsertedAsync(Party party, long stickerId)
    {
        foreach (var client in party.Clients)
        {
            Assert.Equal(stickerId, (await client.ExpectAsync(Kind.StickerUpserted)).StickerUpserted.Sticker.Id);
        }
    }

    private sealed class UnmeasurableStream(byte[] body) : MemoryStream(body)
    {
        public override bool CanSeek => false;
    }
}
