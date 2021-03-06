package client

import (
	"fmt"

	"github.com/pkg/errors"
	"github.com/urfave/cli/v2"

	anchorpath "github.com/wetware/ww/pkg/util/anchor/path"
)

const emsg = "error listing anchor, likely due to missing host or misspelled anchor"

func ls() *cli.Command {
	return &cli.Command{
		Name:      "ls",
		Usage:     "list cluster elements",
		ArgsUsage: "path",
		Action:    lsAction(),
	}
}

func lsAction() cli.ActionFunc {
	return func(c *cli.Context) error {
		path := c.Args().First()

		if err := validatePath(path); err != nil {
			return errors.Wrap(err, "invalid path")
		}

		cs, err := root.Walk(ctx, anchorpath.Parts(path)).Ls(ctx)
		if err != nil {
			return errors.Wrap(err, emsg)
		}

		for _, anchor := range cs {
			_, _ = fmt.Fprintln(c.App.Writer, anchorpath.Join(anchor.Path()))
		}

		return nil
	}
}

func validatePath(path string) error {
	if path == "" {
		return errors.New("must be a glob argument")
	}

	if path[0] != '/' {
		return errors.New("must specify absolute path")
	}

	return nil
}
