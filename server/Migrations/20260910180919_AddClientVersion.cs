using System;
using Microsoft.EntityFrameworkCore.Migrations;

#nullable disable

namespace Vorcall.Server.Migrations
{
    /// <inheritdoc />
    public partial class AddClientVersion : Migration
    {
        /// <inheritdoc />
        protected override void Up(MigrationBuilder migrationBuilder)
        {
            migrationBuilder.AddColumn<string>(
                name: "last_client_platform",
                table: "users",
                type: "character varying(64)",
                maxLength: 64,
                nullable: true);

            migrationBuilder.AddColumn<string>(
                name: "last_client_version",
                table: "users",
                type: "character varying(64)",
                maxLength: 64,
                nullable: true);

            migrationBuilder.AddColumn<DateTime>(
                name: "last_seen_at",
                table: "users",
                type: "timestamp with time zone",
                nullable: true);
        }

        /// <inheritdoc />
        protected override void Down(MigrationBuilder migrationBuilder)
        {
            migrationBuilder.DropColumn(
                name: "last_client_platform",
                table: "users");

            migrationBuilder.DropColumn(
                name: "last_client_version",
                table: "users");

            migrationBuilder.DropColumn(
                name: "last_seen_at",
                table: "users");
        }
    }
}
